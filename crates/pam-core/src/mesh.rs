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

    /// At most `max_tris` triangles, for interactive preview. Borrows `self`
    /// when it is already within budget.
    ///
    /// Uses vertex clustering: weld vertices on a uniform grid and drop the
    /// triangles that collapse. The surface stays closed, just coarser.
    /// (Keeping every Nth triangle instead leaves see-through confetti of
    /// stray slivers.)
    pub fn simplified(&self, max_tris: usize) -> Cow<'_, Mesh> {
        let n = self.triangle_count();
        if n <= max_tris {
            return Cow::Borrowed(self);
        }
        if max_tris == 0 || !self.bbox.is_valid() {
            return Cow::Owned(self.strided(max_tris));
        }
        // Surviving triangles scale with the surface cells, i.e. ~res².
        // Guess from the budget, then correct from what the guess produced.
        let mut res = (max_tris as f32 / 2.0).sqrt().clamp(2.0, 1_000_000.0);
        let mut best: Option<Mesh> = None;
        for _ in 0..4 {
            let mesh = self.clustered(res as u32);
            let got = mesh.triangle_count();
            let fits = got <= max_tris && got > 0;
            if fits && best.as_ref().is_none_or(|b| got > b.triangle_count()) {
                // Close enough to the budget; another pass costs a full sweep.
                if got * 10 >= max_tris * 6 {
                    return Cow::Owned(mesh);
                }
                best = Some(mesh);
            }
            let scale = if got == 0 {
                2.0
            } else {
                (max_tris as f32 / got as f32).sqrt() * 0.95
            };
            res = (res * scale).clamp(2.0, 1_000_000.0);
        }
        Cow::Owned(best.unwrap_or_else(|| self.strided(max_tris)))
    }

    /// Weld vertices into a `res`-cells-per-longest-side grid (cluster
    /// position = member average) and keep only non-degenerate triangles.
    fn clustered(&self, res: u32) -> Mesh {
        let size = self.bbox.size();
        let longest = size[0].max(size[1]).max(size[2]).max(f32::MIN_POSITIVE);
        let inv_cell = res.max(1) as f32 / longest;
        let dims = res.max(1) as u64 + 1;
        let mut cluster_of = Vec::with_capacity(self.vertices.len());
        let mut ids: std::collections::HashMap<u64, u32> = std::collections::HashMap::new();
        let mut sums: Vec<[f64; 3]> = Vec::new();
        let mut counts: Vec<u32> = Vec::new();
        for v in &self.vertices {
            let cell = |i: usize| (((v[i] - self.bbox.min[i]) * inv_cell) as u64).min(dims - 1);
            let key = cell(0) + dims * (cell(1) + dims * cell(2));
            let id = *ids.entry(key).or_insert_with(|| {
                sums.push([0.0; 3]);
                counts.push(0);
                (sums.len() - 1) as u32
            });
            let sum = &mut sums[id as usize];
            for i in 0..3 {
                sum[i] += v[i] as f64;
            }
            counts[id as usize] += 1;
            cluster_of.push(id);
        }
        let mut bbox = BBox::empty();
        let vertices: Vec<[f32; 3]> = sums
            .iter()
            .zip(&counts)
            .map(|(s, &c)| {
                let c = c as f64;
                let v = [(s[0] / c) as f32, (s[1] / c) as f32, (s[2] / c) as f32];
                bbox.include(v);
                v
            })
            .collect();
        let mut indices = Vec::new();
        for tri in self.indices.chunks_exact(3) {
            let [a, b, c] = [0, 1, 2].map(|k| cluster_of[tri[k] as usize]);
            if a != b && b != c && a != c {
                indices.extend_from_slice(&[a, b, c]);
            }
        }
        Mesh {
            vertices,
            indices,
            bbox,
        }
    }

    /// Every Nth triangle. Only a fallback: it leaves holes.
    fn strided(&self, max_tris: usize) -> Mesh {
        let n = self.triangle_count();
        let stride = n.div_ceil(max_tris.max(1)).max(1);
        let mut vertices = Vec::new();
        let mut indices = Vec::new();
        let mut bbox = BBox::empty();
        for t in (0..n).step_by(stride).take(max_tris) {
            for k in 0..3 {
                let v = self.vertices[self.indices[t * 3 + k] as usize];
                indices.push(vertices.len() as u32);
                bbox.include(v);
                vertices.push(v);
            }
        }
        Mesh {
            vertices,
            indices,
            bbox,
        }
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

    /// A finely tessellated closed box (triangle soup, like a binary STL).
    fn dense_box(steps: usize) -> Mesh {
        let mut tris = Vec::new();
        let d = 10.0 / steps as f32;
        for axis in 0..3 {
            for side in [0.0f32, 10.0] {
                for i in 0..steps {
                    for j in 0..steps {
                        let p = |u: usize, v: usize| {
                            let mut q = [0.0; 3];
                            q[axis] = side;
                            q[(axis + 1) % 3] = u as f32 * d;
                            q[(axis + 2) % 3] = v as f32 * d;
                            q
                        };
                        tris.push([p(i, j), p(i + 1, j), p(i + 1, j + 1)]);
                        tris.push([p(i, j), p(i + 1, j + 1), p(i, j + 1)]);
                    }
                }
            }
        }
        Mesh::from_triangles(&tris)
    }

    #[test]
    fn simplify_clusters_instead_of_dropping_triangles() {
        let mesh = dense_box(100); // 120k triangles
        let slim = mesh.simplified(5_000);
        let n = slim.triangle_count();
        assert!((2_000..=5_000).contains(&n), "got {n}");
        // Same extent: clustering moves vertices, it doesn't carve pieces out.
        for i in 0..3 {
            assert!(
                slim.bbox.min[i] < 0.5 && slim.bbox.max[i] > 9.5,
                "{:?}",
                slim.bbox
            );
        }
        // Closed surface in, closed surface out: every edge still has a twin.
        let mut edges = std::collections::HashMap::<(u32, u32), i32>::new();
        for t in slim.indices.chunks_exact(3) {
            for (a, b) in [(t[0], t[1]), (t[1], t[2]), (t[2], t[0])] {
                *edges.entry((a.min(b), a.max(b))).or_default() += 1;
            }
        }
        let open = edges.values().filter(|&&c| c % 2 == 1).count();
        assert_eq!(open, 0, "simplified box has {open} boundary edges (holes)");
    }
}
