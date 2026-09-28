use std::borrow::Cow;
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssetFormat {
    Stl,
    Obj,
    ThreeMf,
}

impl AssetFormat {
    pub fn from_path(path: &Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        match ext.as_str() {
            "stl" => Some(Self::Stl),
            "obj" => Some(Self::Obj),
            "3mf" => Some(Self::ThreeMf),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stl => "stl",
            Self::Obj => "obj",
            Self::ThreeMf => "threemf",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "stl" => Some(Self::Stl),
            "obj" => Some(Self::Obj),
            "threemf" => Some(Self::ThreeMf),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Stl => "STL",
            Self::Obj => "OBJ",
            Self::ThreeMf => "3MF",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BBox {
    pub min: [f32; 3],
    pub max: [f32; 3],
}

impl BBox {
    pub fn empty() -> Self {
        Self {
            min: [f32::INFINITY; 3],
            max: [f32::NEG_INFINITY; 3],
        }
    }

    pub fn include(&mut self, p: [f32; 3]) {
        for i in 0..3 {
            self.min[i] = self.min[i].min(p[i]);
            self.max[i] = self.max[i].max(p[i]);
        }
    }

    pub fn is_valid(&self) -> bool {
        self.min.iter().all(|v| v.is_finite()) && self.max.iter().all(|v| v.is_finite())
    }

    pub fn size(&self) -> [f32; 3] {
        [
            (self.max[0] - self.min[0]).max(0.0),
            (self.max[1] - self.min[1]).max(0.0),
            (self.max[2] - self.min[2]).max(0.0),
        ]
    }

    pub fn center(&self) -> [f32; 3] {
        [
            (self.min[0] + self.max[0]) * 0.5,
            (self.min[1] + self.max[1]) * 0.5,
            (self.min[2] + self.max[2]) * 0.5,
        ]
    }

    pub fn diagonal(&self) -> f32 {
        let s = self.size();
        (s[0] * s[0] + s[1] * s[1] + s[2] * s[2]).sqrt()
    }

    pub fn format_mm(&self) -> String {
        let s = self.size();
        format!("{:.0}×{:.0}×{:.0} mm", s[0], s[1], s[2])
    }
}

#[derive(Clone, Debug)]
pub struct Mesh {
    pub vertices: Vec<[f32; 3]>,
    pub indices: Vec<u32>,
    pub bbox: BBox,
}

impl Mesh {
    pub fn from_triangles(tris: &[[[f32; 3]; 3]]) -> Self {
        let mut vertices = Vec::with_capacity(tris.len() * 3);
        let mut indices = Vec::with_capacity(tris.len() * 3);
        let mut bbox = BBox::empty();
        for tri in tris {
            for v in tri {
                indices.push(vertices.len() as u32);
                bbox.include(*v);
                vertices.push(*v);
            }
        }
        Self {
            vertices,
            indices,
            bbox,
        }
    }

    pub fn triangle_count(&self) -> usize {
        self.indices.len() / 3
    }

    pub fn is_empty(&self) -> bool {
        self.indices.len() < 3
    }

    /// Keep at most `max_tris` triangles by uniform stride. Used for preview.
    /// Borrows `self` when it is already within budget.
    pub fn simplified(&self, max_tris: usize) -> Cow<'_, Mesh> {
        let n = self.triangle_count();
        if n <= max_tris {
            return Cow::Borrowed(self);
        }
        let stride = ((n as f32 / max_tris as f32).ceil() as usize).max(1);
        let mut vertices = Vec::new();
        let mut indices = Vec::new();
        let mut bbox = BBox::empty();
        for t in (0..n).step_by(stride) {
            for k in 0..3 {
                let src = self.indices[t * 3 + k] as usize;
                let v = self.vertices[src];
                indices.push(vertices.len() as u32);
                bbox.include(v);
                vertices.push(v);
            }
        }
        Cow::Owned(Self {
            vertices,
            indices,
            bbox,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn format_from_path_and_label() {
        assert_eq!(
            AssetFormat::from_path(Path::new("a.STL")),
            Some(AssetFormat::Stl)
        );
        assert_eq!(
            AssetFormat::from_path(Path::new("a.obj")),
            Some(AssetFormat::Obj)
        );
        assert_eq!(
            AssetFormat::from_path(Path::new("a.3MF")),
            Some(AssetFormat::ThreeMf)
        );
        assert_eq!(AssetFormat::from_path(Path::new("a.txt")), None);
        assert_eq!(AssetFormat::from_str("threemf"), Some(AssetFormat::ThreeMf));
        assert_eq!(AssetFormat::from_str("nope"), None);
        assert_eq!(AssetFormat::Stl.as_str(), "stl");
        assert_eq!(AssetFormat::Obj.label(), "OBJ");
        assert_eq!(AssetFormat::ThreeMf.label(), "3MF");
    }

    #[test]
    fn bbox_grows_and_reports_size() {
        let mut bbox = BBox::empty();
        assert!(!bbox.is_valid());
        bbox.include([1.0, 2.0, 3.0]);
        bbox.include([-1.0, 4.0, 0.0]);
        assert!(bbox.is_valid());
        assert_eq!(bbox.size(), [2.0, 2.0, 3.0]);
        assert_eq!(bbox.center(), [0.0, 3.0, 1.5]);
        assert!((bbox.diagonal() - (4.0 + 4.0 + 9.0f32).sqrt()).abs() < 1e-5);
        assert_eq!(bbox.format_mm(), "2×2×3 mm");
    }

    #[test]
    fn mesh_simplify_keeps_at_most_max_tris() {
        let tris: Vec<[[f32; 3]; 3]> = (0..12)
            .map(|i| {
                let z = i as f32;
                [[0., 0., z], [1., 0., z], [0., 1., z]]
            })
            .collect();
        let mesh = Mesh::from_triangles(&tris);
        assert_eq!(mesh.triangle_count(), 12);
        assert!(!mesh.is_empty());
        assert_eq!(mesh.simplified(12).triangle_count(), 12);
        assert!(matches!(mesh.simplified(12), Cow::Borrowed(_)));
        let slim = mesh.simplified(4);
        assert!(slim.triangle_count() <= 4);
        assert!(slim.triangle_count() >= 1);
        assert!(Mesh::from_triangles(&[]).is_empty());
    }
}
