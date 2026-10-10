//! What a 3MF project file says about printing it: title and designer from
//! the model, and printer, filament, layer height and (once sliced)
//! estimated time and weight from the slicer's settings. Reads what Bambu
//! Studio, OrcaSlicer and PrusaSlicer write; other 3MF files mostly yield
//! only the model metadata.

use std::io::{BufReader, Read};
use std::path::Path;

use quick_xml::events::Event;
use quick_xml::Reader;

use crate::error::Result;
use crate::load::{attr_str, open_3mf, Package3mf};

/// Bump when [`read_3mf_print_info`] would read existing files differently;
/// the catalog then reads every 3MF again.
pub const PRINT_INFO_VERSION: i64 = 1;

/// Settings files bigger than this are not read (real ones are well under 1 MB).
const MAX_CONFIG_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PrintInfo {
    pub title: Option<String>,
    pub designer: Option<String>,
    /// What wrote the file, e.g. "BambuStudio-01.09.00.70".
    pub application: Option<String>,
    /// Printer preset, e.g. "Bambu Lab A1 mini 0.4 nozzle".
    pub printer: Option<String>,
    /// Filament types such as "PLA" or "PETG", each once, in slot order.
    pub materials: Vec<String>,
    /// Estimated print time of all sliced plates.
    pub print_time_s: Option<u64>,
    /// Estimated filament of all sliced plates.
    pub filament_g: Option<f64>,
    pub layer_height_mm: Option<f32>,
}

impl PrintInfo {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

pub fn read_3mf_print_info(path: &Path) -> Result<PrintInfo> {
    let mut package = open_3mf(path)?;
    let mut info = PrintInfo::default();
    if let Some(root) = package.root.clone() {
        if let Some(model) = read_entry(&mut package, &root, None) {
            read_model_metadata(&model, &mut info);
        }
    }
    let mut slot_materials = Vec::new();
    if let Some(json) = read_text(&mut package, "metadata/project_settings.config") {
        read_project_settings(&json, &mut info, &mut slot_materials);
    } else if let Some(ini) = read_text(&mut package, "metadata/slic3r_pe.config") {
        read_prusa_config(&ini, &mut info, &mut slot_materials);
    }
    if let Some(xml) = read_text(&mut package, "metadata/slice_info.config") {
        read_slice_info(&xml, &mut info);
    }
    // Materials of the sliced plates are what actually prints; the settings
    // list every filament slot of the project.
    if info.materials.is_empty() {
        info.materials = dedup(slot_materials);
    }
    Ok(info)
}

/// The entry's bytes, or `None` if it is missing, unreadable or bigger than
/// `limit` (unlimited for `None`).
fn read_entry(package: &mut Package3mf, name: &str, limit: Option<u64>) -> Option<Vec<u8>> {
    let index = *package.entries.get(name)?;
    let entry = package.archive.by_index(index).ok()?;
    if limit.is_some_and(|max| entry.size() > max) {
        return None;
    }
    let mut bytes = Vec::new();
    BufReader::new(entry).read_to_end(&mut bytes).ok()?;
    Some(bytes)
}

fn read_text(package: &mut Package3mf, name: &str) -> Option<String> {
    let bytes = read_entry(package, name, Some(MAX_CONFIG_BYTES))?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

fn non_empty(s: &str) -> Option<String> {
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

fn dedup(items: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for item in items {
        if !out.iter().any(|seen| seen.eq_ignore_ascii_case(&item)) {
            out.push(item);
        }
    }
    out
}

/// `<metadata name="Title">…</metadata>` before the model's `<resources>`,
/// where the spec puts it; stopping there skips the meshes.
fn read_model_metadata(model: &[u8], info: &mut PrintInfo) {
    let mut reader = Reader::from_reader(model);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut current: Option<String> = None;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => match e.local_name().as_ref() {
                b"metadata" => current = attr_str(&e, b"name"),
                b"resources" => return,
                _ => {}
            },
            Ok(Event::Text(t)) => {
                if let (Some(name), Ok(text)) = (current.take(), t.unescape()) {
                    let value = non_empty(&text);
                    match name.to_ascii_lowercase().as_str() {
                        "title" => info.title = info.title.take().or(value),
                        "designer" => info.designer = info.designer.take().or(value),
                        "application" => info.application = info.application.take().or(value),
                        _ => {}
                    }
                }
            }
            Ok(Event::End(_)) => current = None,
            Ok(Event::Eof) | Err(_) => return,
            _ => {}
        }
        buf.clear();
    }
}

/// Bambu Studio / OrcaSlicer `Metadata/project_settings.config`: one JSON
/// object of preset values, mostly strings, some arrays per filament slot.
fn read_project_settings(json: &str, info: &mut PrintInfo, slot_materials: &mut Vec<String>) {
    let Ok(serde_json::Value::Object(settings)) = serde_json::from_str(json) else {
        return;
    };
    let text = |key: &str| match settings.get(key)? {
        serde_json::Value::String(s) => non_empty(s),
        serde_json::Value::Array(items) => items.first()?.as_str().and_then(non_empty),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    };
    info.printer = text("printer_settings_id").or_else(|| text("printer_model"));
    info.layer_height_mm = text("layer_height").and_then(|v| v.parse().ok());
    if let Some(serde_json::Value::Array(types)) = settings.get("filament_type") {
        slot_materials.extend(
            types
                .iter()
                .filter_map(|t| t.as_str())
                .filter_map(non_empty),
        );
    } else if let Some(one) = text("filament_type") {
        slot_materials.extend(one.split(';').filter_map(non_empty));
    }
}

/// PrusaSlicer `Metadata/Slic3r_PE.config`: `; key = value` lines, with
/// per-extruder values joined by `;`.
fn read_prusa_config(ini: &str, info: &mut PrintInfo, slot_materials: &mut Vec<String>) {
    for line in ini.lines() {
        let Some((key, value)) = line.trim_start_matches(';').split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "printer_settings_id" => info.printer = non_empty(value).or(info.printer.take()),
            "printer_model" if info.printer.is_none() => info.printer = non_empty(value),
            "layer_height" => info.layer_height_mm = value.parse().ok(),
            "filament_type" => slot_materials.extend(value.split(';').filter_map(non_empty)),
            _ => {}
        }
    }
}

/// Bambu Studio / OrcaSlicer `Metadata/slice_info.config`, present once a
/// plate was sliced: per `<plate>`, `prediction` (seconds) and `weight`
/// (grams) metadata and a `<filament type=…>` per filament used.
fn read_slice_info(xml: &str, info: &mut PrintInfo) {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut time: Option<u64> = None;
    let mut weight: Option<f64> = None;
    let mut materials = Vec::new();
    let mut in_plate = false;
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) if e.local_name().as_ref() == b"plate" => in_plate = true,
            Ok(Event::End(e)) if e.local_name().as_ref() == b"plate" => in_plate = false,
            Ok(Event::Start(e) | Event::Empty(e)) if in_plate => match e.local_name().as_ref() {
                b"metadata" => {
                    let value = attr_str(&e, b"value").unwrap_or_default();
                    match attr_str(&e, b"key").as_deref() {
                        Some("prediction") => {
                            if let Ok(secs) = value.trim().parse::<f64>() {
                                *time.get_or_insert(0) += secs.max(0.0).round() as u64;
                            }
                        }
                        Some("weight") => {
                            if let Ok(grams) = value.trim().parse::<f64>() {
                                *weight.get_or_insert(0.0) += grams.max(0.0);
                            }
                        }
                        _ => {}
                    }
                }
                b"filament" => {
                    let used = attr_str(&e, b"used_g")
                        .and_then(|g| g.trim().parse::<f64>().ok())
                        .is_none_or(|g| g > 0.0);
                    if let Some(kind) = attr_str(&e, b"type").as_deref().and_then(non_empty) {
                        if used {
                            materials.push(kind);
                        }
                    }
                }
                _ => {}
            },
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    // An unsliced project has plates with zero predictions.
    info.print_time_s = time.filter(|t| *t > 0).or(info.print_time_s);
    info.filament_g = weight.filter(|w| *w > 0.0).or(info.filament_g);
    if !materials.is_empty() {
        info.materials = dedup(materials);
    }
}

/// Write a 3MF holding `files` (name, contents) next to a one-triangle
/// model with `model_metadata` before its resources (used by tests).
#[cfg(test)]
pub(crate) fn write_test_3mf(dest: &Path, model_metadata: &str, files: &[(&str, &str)]) {
    use std::io::Write;
    let mut zip = zip::ZipWriter::new(std::fs::File::create(dest).unwrap());
    let opts = zip::write::SimpleFileOptions::default();
    zip.start_file("3D/3dmodel.model", opts).unwrap();
    write!(
        zip,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<model unit="millimeter" xmlns="http://schemas.microsoft.com/3dmanufacturing/core/2015/02">
  {model_metadata}
  <resources>
    <object id="1" type="model"><mesh>
      <vertices><vertex x="0" y="0" z="0"/><vertex x="1" y="0" z="0"/><vertex x="0" y="1" z="0"/></vertices>
      <triangles><triangle v1="0" v2="1" v3="2"/></triangles>
    </mesh></object>
    <metadata name="Title">not model metadata</metadata>
  </resources>
  <build><item objectid="1"/></build>
</model>"#
    )
    .unwrap();
    for (name, contents) in files {
        zip.start_file(*name, opts).unwrap();
        zip.write_all(contents.as_bytes()).unwrap();
    }
    zip.finish().unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    const BAMBU_SLICE_INFO: &str = r##"<?xml version="1.0" encoding="UTF-8"?>
<config>
  <header><header_item key="X-BBL-Client-Version" value="01.09.00.70"/></header>
  <plate>
    <metadata key="index" value="1"/>
    <metadata key="prediction" value="3600"/>
    <metadata key="weight" value="10.5"/>
    <filament id="1" type="PETG" color="#FFFFFF" used_m="3.4" used_g="10.5"/>
    <filament id="2" type="PLA" color="#000000" used_m="0" used_g="0"/>
  </plate>
  <plate>
    <metadata key="index" value="2"/>
    <metadata key="prediction" value="1825.4"/>
    <metadata key="weight" value="2.25"/>
    <filament id="3" type="TPU" used_g="2.25"/>
  </plate>
</config>"##;

    #[test]
    fn reads_bambu_project_and_sliced_plates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("benchy.3mf");
        write_test_3mf(
            &path,
            r#"<metadata name="Title">3DBenchy &amp; friends</metadata>
               <metadata name="Designer">CreativeTools</metadata>
               <metadata name="Application">BambuStudio-01.09.00.70</metadata>
               <metadata name="Description"></metadata>"#,
            &[
                (
                    "Metadata/project_settings.config",
                    r#"{"printer_model": "Bambu Lab A1 mini",
                        "printer_settings_id": "Bambu Lab A1 mini 0.4 nozzle",
                        "filament_type": ["PLA", "PETG", "PLA"],
                        "layer_height": "0.2"}"#,
                ),
                ("Metadata/slice_info.config", BAMBU_SLICE_INFO),
            ],
        );
        let info = read_3mf_print_info(&path).unwrap();
        assert_eq!(info.title.as_deref(), Some("3DBenchy & friends"));
        assert_eq!(info.designer.as_deref(), Some("CreativeTools"));
        assert_eq!(info.application.as_deref(), Some("BambuStudio-01.09.00.70"));
        assert_eq!(
            info.printer.as_deref(),
            Some("Bambu Lab A1 mini 0.4 nozzle")
        );
        assert_eq!(info.layer_height_mm, Some(0.2));
        assert_eq!(info.print_time_s, Some(5425));
        assert_eq!(info.filament_g, Some(12.75));
        // Only filaments the sliced plates use, not every project slot.
        assert_eq!(info.materials, ["PETG", "TPU"]);
    }

    #[test]
    fn unsliced_project_lists_filament_slots_and_no_estimates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.3mf");
        write_test_3mf(
            &path,
            "",
            &[
                (
                    "Metadata/project_settings.config",
                    r#"{"printer_model": "Bambu Lab X1 Carbon", "filament_type": ["PLA", "pla", "ABS"]}"#,
                ),
                (
                    "Metadata/slice_info.config",
                    r#"<config><plate><metadata key="prediction" value="0"/><metadata key="weight" value="0"/></plate></config>"#,
                ),
            ],
        );
        let info = read_3mf_print_info(&path).unwrap();
        assert_eq!(info.printer.as_deref(), Some("Bambu Lab X1 Carbon"));
        assert_eq!(info.materials, ["PLA", "ABS"]);
        assert_eq!((info.print_time_s, info.filament_g), (None, None));
        assert_eq!(
            info.title, None,
            "metadata inside <resources> is not the model's"
        );
    }

    #[test]
    fn reads_prusaslicer_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("prusa.3mf");
        write_test_3mf(
            &path,
            r#"<metadata name="Application">PrusaSlicer-2.8.1</metadata>"#,
            &[(
                "Metadata/Slic3r_PE.config",
                "; generated by PrusaSlicer 2.8.1\n; filament_type = PETG;PLA\n; layer_height = 0.15\n; printer_model = MK4S\n; printer_settings_id = Original Prusa MK4S 0.4 nozzle\n",
            )],
        );
        let info = read_3mf_print_info(&path).unwrap();
        assert_eq!(info.application.as_deref(), Some("PrusaSlicer-2.8.1"));
        assert_eq!(
            info.printer.as_deref(),
            Some("Original Prusa MK4S 0.4 nozzle")
        );
        assert_eq!(info.layer_height_mm, Some(0.15));
        assert_eq!(info.materials, ["PETG", "PLA"]);
        assert_eq!(info.print_time_s, None);
    }

    #[test]
    fn plain_3mf_is_empty_and_broken_settings_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("plain.3mf");
        crate::load::write_3mf_cube(&plain, false).unwrap();
        assert!(read_3mf_print_info(&plain).unwrap().is_empty());

        let broken = dir.path().join("broken.3mf");
        write_test_3mf(
            &broken,
            "",
            &[
                ("Metadata/project_settings.config", "{ not json"),
                ("Metadata/slice_info.config", "<config><plate>"),
            ],
        );
        assert!(read_3mf_print_info(&broken).unwrap().is_empty());

        let not_zip = dir.path().join("not.3mf");
        std::fs::write(&not_zip, b"nope").unwrap();
        assert!(read_3mf_print_info(&not_zip).is_err());
    }
}
