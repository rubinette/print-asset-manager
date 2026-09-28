use std::collections::HashMap;
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
    let bytes = std::fs::read(path)?;
    if let Some(mesh) = parse_binary_stl(&bytes) {
        return Ok(mesh);
    }
    let indexed = stl_io::read_stl(&mut Cursor::new(&bytes)).map_err(|e| Error::Parse {
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

/// Binary STL, identified by its exact size (`84 + 50 * n`). stl_io decides
/// by a leading `solid `, which SolidWorks and others also write into binary
/// headers, so those files would otherwise fail as malformed ASCII.
fn parse_binary_stl(bytes: &[u8]) -> Option<Mesh> {
    let count = u32::from_le_bytes(bytes.get(80..84)?.try_into().ok()?) as usize;
    if bytes.len() as u64 != 84 + 50 * count as u64 {
        return None;
    }
    let f32_at = |b: &[u8], i: usize| f32::from_le_bytes(b[i..i + 4].try_into().unwrap());
    let tris: Vec<[[f32; 3]; 3]> = bytes[84..]
        .chunks_exact(50)
        .map(|rec| {
            // Skip the 12-byte normal; vertices follow.
            std::array::from_fn(|v| std::array::from_fn(|c| f32_at(rec, 12 + v * 12 + c * 4)))
        })
        .collect();
    Some(Mesh::from_triangles(&tris))
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
    let mut pkg = Package::new(ZipArchive::new(BufReader::new(file))?)?;
    let root = pkg.root_part().ok_or_else(|| Error::Parse {
        path: path.to_path_buf(),
        message: "no 3MF model part found".into(),
    })?;
    pkg.build_mesh(&root).map_err(|e| Error::Parse {
        path: path.to_path_buf(),
        message: e,
    })
}

/// Row-major 3MF affine transform: `m00 m01 m02 m10 m11 m12 m20 m21 m22 m30 m31 m32`.
/// Points are row vectors, so `p' = p * M` with the last row as translation.
type Transform = [f32; 12];

const IDENTITY: Transform = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0];

/// Guards against component cycles in malformed packages.
const MAX_COMPONENT_DEPTH: u32 = 32;

fn parse_transform(s: &str) -> Option<Transform> {
    let mut t = [0.0f32; 12];
    let mut parts = s.split_ascii_whitespace();
    for slot in &mut t {
        *slot = parts.next()?.parse().ok()?;
    }
    parts.next().is_none().then_some(t)
}

fn apply_transform(t: &Transform, [x, y, z]: [f32; 3]) -> [f32; 3] {
    [
        x * t[0] + y * t[3] + z * t[6] + t[9],
        x * t[1] + y * t[4] + z * t[7] + t[10],
        x * t[2] + y * t[5] + z * t[8] + t[11],
    ]
}

/// Transform equivalent to applying `inner` first, then `outer`.
fn compose(inner: &Transform, outer: &Transform) -> Transform {
    let row = |i: usize| apply_transform(outer, [inner[i], inner[i + 1], inner[i + 2]]);
    let linear = |i: usize| {
        let [x, y, z] = [inner[i], inner[i + 1], inner[i + 2]];
        [
            x * outer[0] + y * outer[3] + z * outer[6],
            x * outer[1] + y * outer[4] + z * outer[7],
            x * outer[2] + y * outer[5] + z * outer[8],
        ]
    };
    let [a, b, c, d] = [linear(0), linear(3), linear(6), row(9)];
    [
        a[0], a[1], a[2], b[0], b[1], b[2], c[0], c[1], c[2], d[0], d[1], d[2],
    ]
}

#[derive(Clone)]
struct Component {
    /// Normalized part path (Production extension `p:path`); `None` means same part.
    path: Option<String>,
    object_id: u32,
    transform: Transform,
}

#[derive(Default)]
struct Object {
    vertices: Vec<[f32; 3]>,
    indices: Vec<u32>,
    components: Vec<Component>,
}

#[derive(Default)]
struct ModelPart {
    objects: HashMap<u32, Object>,
    build: Vec<(u32, Transform)>,
}

/// Lowercased, forward-slash, no leading `/` — the key used for part lookup.
fn normalize_part_name(name: &str) -> String {
    name.replace('\\', "/")
        .trim_start_matches('/')
        .to_ascii_lowercase()
}

/// A 3MF package whose model parts are parsed on demand. Bambu Studio / Orca
/// write a root `3D/3dmodel.model` that only references meshes stored in
/// `3D/Objects/*.model` via Production-extension components.
struct Package<R: Read + std::io::Seek> {
    archive: ZipArchive<R>,
    entries: HashMap<String, usize>,
    parts: HashMap<String, ModelPart>,
}

impl<R: Read + std::io::Seek> Package<R> {
    fn new(archive: ZipArchive<R>) -> Result<Self> {
        let entries = (0..archive.len())
            .filter_map(|i| Some((normalize_part_name(archive.name_for_index(i)?), i)))
            .collect();
        Ok(Self {
            archive,
            entries,
            parts: HashMap::new(),
        })
    }

    /// Root model part per `_rels/.rels`, falling back to the conventional
    /// name and then to any `.model` entry.
    fn root_part(&mut self) -> Option<String> {
        if let Some(target) = self.rels_start_part() {
            if self.entries.contains_key(&target) {
                return Some(target);
            }
        }
        if self.entries.contains_key("3d/3dmodel.model") {
            return Some("3d/3dmodel.model".into());
        }
        let mut models: Vec<_> = self
            .entries
            .keys()
            .filter(|n| n.ends_with(".model"))
            .cloned()
            .collect();
        models.sort();
        models.into_iter().next()
    }

    fn rels_start_part(&mut self) -> Option<String> {
        let idx = *self.entries.get("_rels/.rels")?;
        let entry = self.archive.by_index(idx).ok()?;
        let mut reader = Reader::from_reader(BufReader::new(entry));
        let mut buf = Vec::new();
        loop {
            match reader.read_event_into(&mut buf).ok()? {
                Event::Start(e) | Event::Empty(e) if e.local_name().as_ref() == b"Relationship" => {
                    let mut target = None;
                    let mut is_model = false;
                    for attr in e.attributes().flatten() {
                        let val = attr.unescape_value().ok()?;
                        match attr.key.local_name().as_ref() {
                            b"Target" => target = Some(normalize_part_name(&val)),
                            b"Type" => is_model = val.ends_with("/3dmodel"),
                            _ => {}
                        }
                    }
                    if is_model {
                        return target;
                    }
                }
                Event::Eof => return None,
                _ => {}
            }
            buf.clear();
        }
    }

    fn ensure_part(&mut self, name: &str) -> std::result::Result<(), String> {
        if self.parts.contains_key(name) {
            return Ok(());
        }
        let idx = *self
            .entries
            .get(name)
            .ok_or_else(|| format!("missing model part: {name}"))?;
        let entry = self.archive.by_index(idx).map_err(|e| e.to_string())?;
        let part = parse_model_part(BufReader::new(entry))?;
        self.parts.insert(name.to_string(), part);
        Ok(())
    }

    fn build_mesh(&mut self, root: &str) -> std::result::Result<Mesh, String> {
        self.ensure_part(root)?;
        let root_part = &self.parts[root];
        let items = if root_part.build.is_empty() {
            // `<build>` is required, but be lenient: show every root object.
            root_part.objects.keys().map(|&id| (id, IDENTITY)).collect()
        } else {
            root_part.build.clone()
        };
        let mut mesh = Mesh {
            vertices: Vec::new(),
            indices: Vec::new(),
            bbox: crate::mesh::BBox::empty(),
        };
        for (id, transform) in items {
            self.emit(root, id, &transform, &mut mesh, 0)?;
        }
        Ok(mesh)
    }

    fn emit(
        &mut self,
        part: &str,
        id: u32,
        transform: &Transform,
        out: &mut Mesh,
        depth: u32,
    ) -> std::result::Result<(), String> {
        if depth > MAX_COMPONENT_DEPTH {
            return Err("3MF components nested too deeply".into());
        }
        self.ensure_part(part)?;
        let Some(obj) = self.parts[part].objects.get(&id) else {
            return Ok(());
        };
        let base = out.vertices.len() as u32;
        let n = obj.vertices.len() as u32;
        for &v in &obj.vertices {
            let v = apply_transform(transform, v);
            out.bbox.include(v);
            out.vertices.push(v);
        }
        for tri in obj.indices.chunks_exact(3) {
            if tri.iter().all(|&i| i < n) {
                out.indices.extend(tri.iter().map(|&i| base + i));
            }
        }
        let components = obj.components.clone();
        for c in components {
            let child_part = c.path.as_deref().unwrap_or(part).to_string();
            let t = compose(&c.transform, transform);
            self.emit(&child_part, c.object_id, &t, out, depth + 1)?;
        }
        Ok(())
    }
}

fn attr_str(e: &quick_xml::events::BytesStart, name: &[u8]) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.local_name().as_ref() == name)
        .and_then(|a| a.unescape_value().ok().map(|v| v.into_owned()))
}

fn attr_parse<T: std::str::FromStr>(e: &quick_xml::events::BytesStart, name: &[u8]) -> Option<T> {
    attr_str(e, name)?.trim().parse().ok()
}

fn parse_model_part(src: impl std::io::BufRead) -> std::result::Result<ModelPart, String> {
    let mut reader = Reader::from_reader(src);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut part = ModelPart::default();
    let mut current: Option<(u32, Object)> = None;

    loop {
        let event = reader
            .read_event_into(&mut buf)
            .map_err(|e| e.to_string())?;
        let (e, is_empty) = match &event {
            Event::Start(e) => (e, false),
            Event::Empty(e) => (e, true),
            Event::End(e) => {
                if e.local_name().as_ref() == b"object" {
                    if let Some((id, obj)) = current.take() {
                        part.objects.insert(id, obj);
                    }
                }
                buf.clear();
                continue;
            }
            Event::Eof => break,
            _ => {
                buf.clear();
                continue;
            }
        };
        match e.local_name().as_ref() {
            b"object" => {
                let id = attr_parse(e, b"id").unwrap_or(0);
                if is_empty {
                    part.objects.insert(id, Object::default());
                } else {
                    current = Some((id, Object::default()));
                }
            }
            b"vertex" => {
                if let Some((_, obj)) = current.as_mut() {
                    obj.vertices.push([
                        attr_parse(e, b"x").unwrap_or(0.0),
                        attr_parse(e, b"y").unwrap_or(0.0),
                        attr_parse(e, b"z").unwrap_or(0.0),
                    ]);
                }
            }
            b"triangle" => {
                if let Some((_, obj)) = current.as_mut() {
                    obj.indices.extend_from_slice(&[
                        attr_parse(e, b"v1").unwrap_or(0),
                        attr_parse(e, b"v2").unwrap_or(0),
                        attr_parse(e, b"v3").unwrap_or(0),
                    ]);
                }
            }
            b"component" => {
                if let (Some((_, obj)), Some(object_id)) =
                    (current.as_mut(), attr_parse(e, b"objectid"))
                {
                    obj.components.push(Component {
                        path: attr_str(e, b"path").map(|p| normalize_part_name(&p)),
                        object_id,
                        transform: attr_str(e, b"transform")
                            .and_then(|t| parse_transform(&t))
                            .unwrap_or(IDENTITY),
                    });
                }
            }
            b"item" => {
                if let Some(object_id) = attr_parse(e, b"objectid") {
                    let transform = attr_str(e, b"transform")
                        .and_then(|t| parse_transform(&t))
                        .unwrap_or(IDENTITY);
                    part.build.push((object_id, transform));
                }
            }
            _ => {}
        }
        buf.clear();
    }

    Ok(part)
}

/// Pull the best embedded PNG thumbnail out of a 3MF package, if any.
pub fn extract_3mf_thumbnail(path: &Path) -> Result<Option<Vec<u8>>> {
    let file = File::open(path)?;
    let mut archive = ZipArchive::new(BufReader::new(file))?;
    // Rank by name and declared size first so only the winner is decompressed.
    let mut candidates = Vec::new();
    for i in 0..archive.len() {
        let entry = archive.by_index_raw(i)?;
        let name = entry.name().replace('\\', "/").to_ascii_lowercase();
        if name.ends_with(".png") {
            candidates.push((thumbnail_score(&name, entry.size() as usize), i));
        }
    }
    candidates.sort_by_key(|c| std::cmp::Reverse(c.0));
    for (_, i) in candidates {
        let mut bytes = Vec::new();
        archive.by_index(i)?.read_to_end(&mut bytes)?;
        if bytes.len() >= 16 && &bytes[0..8] == b"\x89PNG\r\n\x1a\n" {
            return Ok(Some(bytes));
        }
    }
    Ok(None)
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
    fn binary_stl_with_solid_header() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sw.STL");
        let mut bytes = Vec::new();
        let mut header = b"solid exported by SolidWorks".to_vec();
        header.resize(80, b' ');
        bytes.extend_from_slice(&header);
        bytes.extend_from_slice(&1u32.to_le_bytes());
        for v in [
            0.0f32, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 2.0, 0.0,
        ] {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        bytes.extend_from_slice(&[0, 0]);
        std::fs::write(&path, &bytes).unwrap();
        let mesh = load_mesh(&path).unwrap();
        assert_eq!(mesh.triangle_count(), 1);
        assert_eq!(mesh.bbox.max, [1.0, 2.0, 0.0]);
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

    #[test]
    fn empty_obj_is_empty_mesh_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.obj");
        std::fs::write(&path, "o empty\n").unwrap();
        match load_mesh(&path) {
            Err(Error::EmptyMesh(p)) => assert_eq!(p, path),
            other => panic!("expected EmptyMesh, got {other:?}"),
        }
    }

    #[test]
    fn unsupported_extension_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("model.step");
        std::fs::write(&path, "ISO-10303").unwrap();
        match load_mesh(&path) {
            Err(Error::UnsupportedFormat(s)) => assert!(s.ends_with("model.step")),
            other => panic!("expected UnsupportedFormat, got {other:?}"),
        }
    }

    #[test]
    fn threemf_thumb_prefers_plate_and_skips_invalid_png() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("multi.3mf");
        let plate = tiny_png();
        let big = {
            let img = image::RgbaImage::from_pixel(64, 64, image::Rgba([1, 2, 3, 255]));
            let mut buf = Cursor::new(Vec::new());
            img.write_to(&mut buf, image::ImageFormat::Png).unwrap();
            buf.into_inner()
        };
        let mut zip = zip::ZipWriter::new(File::create(&path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        // Highest score by name and size, but not actually a PNG.
        zip.start_file("Metadata/plate_2.png", opts).unwrap();
        zip.write_all(&[0u8; 4096]).unwrap();
        zip.start_file("Metadata/plate_1.png", opts).unwrap();
        zip.write_all(&plate).unwrap();
        // Larger than plate_1, but a weaker name.
        zip.start_file("Auxiliaries/other.png", opts).unwrap();
        zip.write_all(&big).unwrap();
        zip.finish().unwrap();

        assert_eq!(extract_3mf_thumbnail(&path).unwrap().unwrap(), plate);
    }

    #[test]
    fn threemf_without_embedded_thumb() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cube.3mf");
        write_3mf_cube(&path, false).unwrap();
        assert!(extract_3mf_thumbnail(&path).unwrap().is_none());
        assert_eq!(load_mesh(&path).unwrap().triangle_count(), 12);
    }

    /// Bambu Studio layout: root part holds only a component pointing into
    /// `3D/Objects/object_1.model`, with transforms on both item and component.
    #[test]
    fn threemf_production_components_across_parts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bambu.3mf");
        let root = r#"<?xml version="1.0" encoding="UTF-8"?>
<model unit="millimeter" xmlns="http://schemas.microsoft.com/3dmanufacturing/core/2015/02" xmlns:p="http://schemas.microsoft.com/3dmanufacturing/production/2015/06" requiredextensions="p">
 <resources>
  <object id="2" type="model">
   <components>
    <component p:path="/3D/Objects/object_1.model" objectid="1" transform="1 0 0 0 1 0 0 0 1 10 0 0"/>
    <component p:path="/3D/Objects/object_1.model" objectid="1" transform="2 0 0 0 2 0 0 0 2 0 0 0"/>
   </components>
  </object>
 </resources>
 <build><item objectid="2" transform="1 0 0 0 1 0 0 0 1 0 0 5"/></build>
</model>"#;
        let object = r#"<?xml version="1.0" encoding="UTF-8"?>
<model unit="millimeter" xmlns="http://schemas.microsoft.com/3dmanufacturing/core/2015/02">
 <resources>
  <object id="1" type="model">
   <mesh>
    <vertices>
     <vertex x="0" y="0" z="0"/><vertex x="1" y="0" z="0"/><vertex x="0" y="1" z="0"/>
    </vertices>
    <triangles><triangle v1="0" v2="1" v3="2"/><triangle v1="0" v2="1" v3="9"/></triangles>
   </mesh>
  </object>
 </resources>
</model>"#;
        let rels = r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
 <Relationship Target="/3D/3dmodel.model" Id="rel-1" Type="http://schemas.microsoft.com/3dmanufacturing/2013/01/3dmodel"/>
</Relationships>"#;
        let mut zip = zip::ZipWriter::new(File::create(&path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        zip.start_file("_rels/.rels", opts).unwrap();
        zip.write_all(rels.as_bytes()).unwrap();
        // Stored before the root on purpose: the loader must not just take the first `.model`.
        zip.start_file("3D/Objects/object_1.model", opts).unwrap();
        zip.write_all(object.as_bytes()).unwrap();
        zip.start_file("3D/3dmodel.model", opts).unwrap();
        zip.write_all(root.as_bytes()).unwrap();
        zip.finish().unwrap();

        let mesh = load_mesh(&path).unwrap();
        // One valid triangle per component; the out-of-range one is dropped.
        assert_eq!(mesh.triangle_count(), 2);
        assert_eq!(mesh.bbox.min, [0.0, 0.0, 5.0]);
        assert_eq!(mesh.bbox.max, [11.0, 2.0, 5.0]);
    }

    #[test]
    fn transform_compose_matches_sequential_apply() {
        let inner = [0.0, 1.0, 0.0, -1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 3.0, 0.0, 0.0];
        let outer = [2.0, 0.0, 0.0, 0.0, 2.0, 0.0, 0.0, 0.0, 2.0, 0.0, 7.0, 1.0];
        let p = [1.0, 2.0, 3.0];
        assert_eq!(
            apply_transform(&compose(&inner, &outer), p),
            apply_transform(&outer, apply_transform(&inner, p))
        );
        assert_eq!(parse_transform("1 0 0 0 1 0 0 0 1 0 0"), None);
    }

    #[test]
    fn zip_without_model_part_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.3mf");
        let file = File::create(&path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        zip.start_file("readme.txt", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"no model").unwrap();
        zip.finish().unwrap();
        assert!(load_mesh(&path).is_err());
    }

    /// Point `PAM_3MF` at a real file to smoke-test it:
    /// `PAM_3MF=/path/x.3mf cargo test -p pam-core --release real_3mf -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn real_3mf() {
        let path = std::path::PathBuf::from(std::env::var("PAM_3MF").unwrap());
        let t = std::time::Instant::now();
        let mesh = load_mesh(&path).unwrap();
        println!(
            "{} tris, bbox {}, {:?}",
            mesh.triangle_count(),
            mesh.bbox.format_mm(),
            t.elapsed()
        );
    }

    #[test]
    fn testdata_cube_stl_loads() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/cube.stl");
        let mesh = load_mesh(&path).unwrap();
        assert!(mesh.triangle_count() >= 12);
        assert!(mesh.bbox.is_valid());
    }
}
