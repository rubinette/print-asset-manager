use std::fs::File;
use std::io::{BufReader, Cursor, Read};
use std::path::Path;

use quick_xml::events::Event;
use quick_xml::Reader;
use zip::ZipArchive;

use crate::error::{Error, Result};
use crate::mesh::{AssetFormat, Mesh};

pub fn load_mesh(path: &Path) -> Result<Mesh> {
    let format = AssetFormat::from_path(path)
        .ok_or_else(|| Error::UnsupportedFormat(path.display().to_string()))?;
    let mesh = match format {
        AssetFormat::Stl => load_stl(path)?,
        AssetFormat::Obj => load_obj(path)?,
        AssetFormat::ThreeMf => load_3mf(path)?,
    };
    if mesh.is_empty() {
        return Err(Error::EmptyMesh(path.to_path_buf()));
    }
    Ok(mesh)
}

fn load_stl(path: &Path) -> Result<Mesh> {
    let mut file = File::open(path)?;
    let indexed = stl_io::read_stl(&mut file).map_err(|e| Error::Parse {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;
    let vertices: Vec<[f32; 3]> = indexed
        .vertices
        .iter()
        .map(|v| [v[0], v[1], v[2]])
        .collect();
    let mut indices = Vec::with_capacity(indexed.faces.len() * 3);
    let mut bbox = crate::mesh::BBox::empty();
    for v in &vertices {
        bbox.include(*v);
    }
    for face in &indexed.faces {
        indices.extend_from_slice(&[
            face.vertices[0] as u32,
            face.vertices[1] as u32,
            face.vertices[2] as u32,
        ]);
    }
    Ok(Mesh {
        vertices,
        indices,
        bbox,
    })
}

fn load_obj(path: &Path) -> Result<Mesh> {
    let (models, _mats) = tobj::load_obj(
        path,
        &tobj::LoadOptions {
            triangulate: true,
            single_index: true,
            ..Default::default()
        },
    )
    .map_err(|e| Error::Parse {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;

    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    let mut bbox = crate::mesh::BBox::empty();
    for model in models {
        let mesh = model.mesh;
        let base = vertices.len() as u32;
        let pos = &mesh.positions;
        for chunk in pos.chunks(3) {
            if chunk.len() < 3 {
                break;
            }
            let v = [chunk[0], chunk[1], chunk[2]];
            bbox.include(v);
            vertices.push(v);
        }
        for idx in mesh.indices {
            indices.push(base + idx);
        }
    }
    Ok(Mesh {
        vertices,
        indices,
        bbox,
    })
}

fn load_3mf(path: &Path) -> Result<Mesh> {
    let file = File::open(path)?;
    let mut archive = ZipArchive::new(BufReader::new(file))?;
    let mut model_xml = None;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let name = entry.name().replace('\\', "/");
        if name.to_ascii_lowercase().ends_with(".model") {
            let mut buf = String::new();
            entry.read_to_string(&mut buf)?;
            model_xml = Some(buf);
            break;
        }
    }
    let xml = model_xml.ok_or_else(|| Error::Parse {
        path: path.to_path_buf(),
        message: "no 3MF model part found".into(),
    })?;
    parse_3mf_model(&xml).map_err(|e| Error::Parse {
        path: path.to_path_buf(),
        message: e,
    })
}

fn parse_3mf_model(xml: &str) -> std::result::Result<Mesh, String> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    let mut bbox = crate::mesh::BBox::empty();
    let mut object_base = 0u32;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e) | Event::Empty(e)) => {
                let local = String::from_utf8_lossy(e.local_name().as_ref()).to_string();
                match local.as_str() {
                    "object" => {
                        object_base = vertices.len() as u32;
                    }
                    "vertex" => {
                        let mut x = 0.0f32;
                        let mut y = 0.0f32;
                        let mut z = 0.0f32;
                        for attr in e.attributes().flatten() {
                            let key = attr.key.local_name();
                            let val = attr
                                .unescape_value()
                                .map_err(|err| err.to_string())?
                                .parse::<f32>()
                                .unwrap_or(0.0);
                            match key.as_ref() {
                                b"x" => x = val,
                                b"y" => y = val,
                                b"z" => z = val,
                                _ => {}
                            }
                        }
                        let v = [x, y, z];
                        bbox.include(v);
                        vertices.push(v);
                    }
                    "triangle" => {
                        let mut v1 = 0u32;
                        let mut v2 = 0u32;
                        let mut v3 = 0u32;
                        for attr in e.attributes().flatten() {
                            let key = attr.key.local_name();
                            let val = attr
                                .unescape_value()
                                .map_err(|err| err.to_string())?
                                .parse::<u32>()
                                .unwrap_or(0);
                            match key.as_ref() {
                                b"v1" => v1 = val,
                                b"v2" => v2 = val,
                                b"v3" => v3 = val,
                                _ => {}
                            }
                        }
                        indices.extend_from_slice(&[
                            object_base + v1,
                            object_base + v2,
                            object_base + v3,
                        ]);
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(e.to_string()),
            _ => {}
        }
        buf.clear();
    }

    Ok(Mesh {
        vertices,
        indices,
        bbox,
    })
}

/// Pull the best embedded PNG thumbnail out of a 3MF package, if any.
pub fn extract_3mf_thumbnail(path: &Path) -> Result<Option<Vec<u8>>> {
    let file = File::open(path)?;
    let mut archive = ZipArchive::new(BufReader::new(file))?;
    let mut best: Option<(usize, Vec<u8>)> = None;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let name = entry.name().replace('\\', "/").to_ascii_lowercase();
        if !name.ends_with(".png") {
            continue;
        }
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes)?;
        if bytes.len() < 16 || &bytes[0..8] != b"\x89PNG\r\n\x1a\n" {
            continue;
        }
        let score = thumbnail_score(&name, bytes.len());
        if best.as_ref().is_none_or(|(s, _)| score > *s) {
            best = Some((score, bytes));
        }
    }
    Ok(best.map(|(_, b)| b))
}

fn thumbnail_score(name: &str, size: usize) -> usize {
    let mut score = size;
    if name.contains("metadata/plate_") {
        score += 10_000_000;
    } else if name.contains("thumbnail") {
        score += 5_000_000;
    } else if name.contains("preview") {
        score += 2_000_000;
    }
    score
}

/// Build a tiny in-memory 3MF (used by tests).
pub fn write_3mf_cube(dest: &Path, with_thumb: bool) -> Result<()> {
    let model = r#"<?xml version="1.0" encoding="UTF-8"?>
<model unit="millimeter" xml:lang="en-US" xmlns="http://schemas.microsoft.com/3dmanufacturing/core/2015/02">
  <resources>
    <object id="1" type="model">
      <mesh>
        <vertices>
          <vertex x="0" y="0" z="0"/>
          <vertex x="10" y="0" z="0"/>
          <vertex x="10" y="10" z="0"/>
          <vertex x="0" y="10" z="0"/>
          <vertex x="0" y="0" z="10"/>
          <vertex x="10" y="0" z="10"/>
          <vertex x="10" y="10" z="10"/>
          <vertex x="0" y="10" z="10"/>
        </vertices>
        <triangles>
          <triangle v1="0" v2="1" v3="2"/>
          <triangle v1="0" v2="2" v3="3"/>
          <triangle v1="4" v2="6" v3="5"/>
          <triangle v1="4" v2="7" v3="6"/>
          <triangle v1="0" v2="4" v3="5"/>
          <triangle v1="0" v2="5" v3="1"/>
          <triangle v1="1" v2="5" v3="6"/>
          <triangle v1="1" v2="6" v3="2"/>
          <triangle v1="2" v2="6" v3="7"/>
          <triangle v1="2" v2="7" v3="3"/>
          <triangle v1="3" v2="7" v3="4"/>
          <triangle v1="3" v2="4" v3="0"/>
        </triangles>
      </mesh>
    </object>
  </resources>
  <build>
    <item objectid="1"/>
  </build>
</model>
"#;
    let file = File::create(dest)?;
    let mut zip = zip::ZipWriter::new(file);
    let opts = zip::write::SimpleFileOptions::default();
    zip.start_file("3D/3dmodel.model", opts)?;
    std::io::Write::write_all(&mut zip, model.as_bytes())?;
    if with_thumb {
        zip.start_file("Metadata/plate_1.png", opts)?;
        std::io::Write::write_all(&mut zip, &tiny_png())?;
    }
    zip.finish()?;
    Ok(())
}

fn tiny_png() -> Vec<u8> {
    let img = image::RgbaImage::from_pixel(8, 8, image::Rgba([40, 140, 220, 255]));
    let mut buf = Cursor::new(Vec::new());
    img.write_to(&mut buf, image::ImageFormat::Png).unwrap();
    buf.into_inner()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn ascii_cube_stl() -> &'static str {
        r#"solid cube
  facet normal 0 0 -1
    outer loop
      vertex 0 0 0
      vertex 1 0 0
      vertex 1 1 0
    endloop
  endfacet
  facet normal 0 0 -1
    outer loop
      vertex 0 0 0
      vertex 1 1 0
      vertex 0 1 0
    endloop
  endfacet
  facet normal 0 0 1
    outer loop
      vertex 0 0 1
      vertex 1 1 1
      vertex 1 0 1
    endloop
  endfacet
  facet normal 0 0 1
    outer loop
      vertex 0 0 1
      vertex 0 1 1
      vertex 1 1 1
    endloop
  endfacet
  facet normal 0 -1 0
    outer loop
      vertex 0 0 0
      vertex 1 0 1
      vertex 1 0 0
    endloop
  endfacet
  facet normal 0 -1 0
    outer loop
      vertex 0 0 0
      vertex 0 0 1
      vertex 1 0 1
    endloop
  endfacet
  facet normal 0 1 0
    outer loop
      vertex 0 1 0
      vertex 1 1 0
      vertex 1 1 1
    endloop
  endfacet
  facet normal 0 1 0
    outer loop
      vertex 0 1 0
      vertex 1 1 1
      vertex 0 1 1
    endloop
  endfacet
  facet normal -1 0 0
    outer loop
      vertex 0 0 0
      vertex 0 1 0
      vertex 0 1 1
    endloop
  endfacet
  facet normal -1 0 0
    outer loop
      vertex 0 0 0
      vertex 0 1 1
      vertex 0 0 1
    endloop
  endfacet
  facet normal 1 0 0
    outer loop
      vertex 1 0 0
      vertex 1 1 1
      vertex 1 1 0
    endloop
  endfacet
  facet normal 1 0 0
    outer loop
      vertex 1 0 0
      vertex 1 0 1
      vertex 1 1 1
    endloop
  endfacet
endsolid cube
"#
    }

    #[test]
    fn stl_ascii_cube() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cube.stl");
        std::fs::write(&path, ascii_cube_stl()).unwrap();
        let mesh = load_mesh(&path).unwrap();
        assert_eq!(mesh.triangle_count(), 12);
        assert!(mesh.bbox.is_valid());
    }

    #[test]
    fn bad_stl_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.stl");
        std::fs::write(&path, "this is not an stl").unwrap();
        assert!(load_mesh(&path).is_err());
    }

    #[test]
    fn obj_triangle() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tri.obj");
        let mut f = File::create(&path).unwrap();
        writeln!(f, "v 0 0 0\nv 1 0 0\nv 0 1 0\nf 1 2 3").unwrap();
        let mesh = load_mesh(&path).unwrap();
        assert_eq!(mesh.triangle_count(), 1);
    }

    #[test]
    fn threemf_cube_and_thumb() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cube.3mf");
        write_3mf_cube(&path, true).unwrap();
        let mesh = load_mesh(&path).unwrap();
        assert_eq!(mesh.triangle_count(), 12);
        let thumb = extract_3mf_thumbnail(&path).unwrap().unwrap();
        assert!(thumb.starts_with(b"\x89PNG"));
    }
}
