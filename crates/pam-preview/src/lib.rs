use image::RgbaImage;
use pam_core::Mesh;

const BG: [u8; 4] = [18, 18, 22, 255];
const DIFFUSE: [f32; 3] = [0.55, 0.72, 0.88];

#[derive(Clone, Copy, Debug)]
pub struct Camera {
    pub yaw: f32,
    pub pitch: f32,
    pub distance: f32,
}

impl Default for Camera {
    fn default() -> Self {
        Self {
            yaw: 45f32.to_radians(),
            pitch: 32f32.to_radians(),
            distance: 1.8,
        }
    }
}

impl Camera {
    pub fn isometric() -> Self {
        Self::default()
    }

    pub fn orbit(&mut self, dx: f32, dy: f32) {
        self.yaw += dx * 0.01;
        self.pitch = (self.pitch + dy * 0.01).clamp(-1.45, 1.45);
    }

    pub fn zoom(&mut self, delta: f32) {
        self.distance = (self.distance * (1.0 - delta * 0.001)).clamp(0.4, 12.0);
    }
}

pub fn render_thumbnail(mesh: &Mesh, size: u32) -> RgbaImage {
    render_mesh(mesh, &Camera::isometric(), size, size, BG)
}

pub fn render_mesh(
    mesh: &Mesh,
    camera: &Camera,
    width: u32,
    height: u32,
    background: [u8; 4],
) -> RgbaImage {
    // Renders every triangle; callers pass a `Mesh::simplified` LOD when they
    // need speed (e.g. while dragging). ~100 ms for 3M triangles at 480 px.
    let w = width.max(1) as i32;
    let h = height.max(1) as i32;
    let mut color = vec![background; (w * h) as usize];
    // 1/depth per pixel; 0 = nothing drawn yet, larger = nearer.
    let mut zbuf = vec![0.0f32; (w * h) as usize];

    if mesh.is_empty() || !mesh.bbox.is_valid() {
        return to_image(width, height, &color);
    }

    let center = mesh.bbox.center();
    let radius = mesh.bbox.diagonal().max(1e-3) * 0.5;
    let (view, light) = view_matrix(camera, center, radius);
    let aspect = width as f32 / height.max(1) as f32;
    let focal = 1.0 / (40f32.to_radians() * 0.5).tan();
    // Scaled to the model so zooming into it clips cleanly instead of
    // projecting geometry that is behind the camera.
    let near = radius * 1e-3;

    // View space with depth positive in front of the camera.
    let view_verts: Vec<[f32; 3]> = mesh
        .vertices
        .iter()
        .map(|v| {
            let p = mul(view, *v);
            [p[0], p[1], -p[2]]
        })
        .collect();
    // Screen x/y plus 1/depth, which interpolates linearly in screen space
    // and keeps relative precision (NDC z crowds everything next to 1.0, so
    // thin walls z-fought and back faces bled through).
    let to_screen = |p: [f32; 3]| {
        let inv = 1.0 / p[2];
        [
            (p[0] * focal / aspect * inv * 0.5 + 0.5) * (w as f32 - 1.0),
            (1.0 - (p[1] * focal * inv * 0.5 + 0.5)) * (h as f32 - 1.0),
            inv,
        ]
    };

    for tri in mesh.indices.chunks_exact(3) {
        let [i0, i1, i2] = [tri[0] as usize, tri[1] as usize, tri[2] as usize];
        let (wa, wb, wc) = (mesh.vertices[i0], mesh.vertices[i1], mesh.vertices[i2]);
        let e1 = sub(wb, wa);
        let e2 = sub(wc, wa);
        let n = normalize(cross(e1, e2));
        let ndotl = (dot(n, light) * 0.5 + 0.5).clamp(0.12, 1.0);
        let shade = [
            (DIFFUSE[0] * ndotl * 255.0) as u8,
            (DIFFUSE[1] * ndotl * 255.0) as u8,
            (DIFFUSE[2] * ndotl * 255.0) as u8,
            255,
        ];
        let (poly, count) = clip_near([view_verts[i0], view_verts[i1], view_verts[i2]], near);
        for k in 1..count.saturating_sub(1) {
            fill_triangle(
                &mut color,
                &mut zbuf,
                w,
                h,
                to_screen(poly[0]),
                to_screen(poly[k]),
                to_screen(poly[k + 1]),
                shade,
            );
        }
    }

    to_image(width, height, &color)
}

pub fn encode_png(img: &RgbaImage) -> Vec<u8> {
    let mut buf = std::io::Cursor::new(Vec::new());
    img.write_to(&mut buf, image::ImageFormat::Png)
        .expect("png encode");
    buf.into_inner()
}

fn to_image(width: u32, height: u32, color: &[[u8; 4]]) -> RgbaImage {
    if width == 0 || height == 0 {
        return RgbaImage::new(width, height);
    }
    let raw: Vec<u8> = color.iter().flatten().copied().collect();
    RgbaImage::from_raw(width, height, raw).expect("buffer matches dimensions")
}

fn view_matrix(camera: &Camera, center: [f32; 3], radius: f32) -> ([[f32; 4]; 4], [f32; 3]) {
    let dist = radius * camera.distance;
    let cy = camera.yaw.cos();
    let sy = camera.yaw.sin();
    let cp = camera.pitch.cos();
    let sp = camera.pitch.sin();
    let eye = [
        center[0] + dist * cp * cy,
        center[1] + dist * sp,
        center[2] + dist * cp * sy,
    ];
    let up = [0.0, 1.0, 0.0];
    let f = normalize(sub(center, eye));
    let s = normalize(cross(f, up));
    let u = cross(s, f);
    let light = normalize([0.35, 0.8, 0.45]);
    let m = [
        [s[0], u[0], -f[0], 0.0],
        [s[1], u[1], -f[1], 0.0],
        [s[2], u[2], -f[2], 0.0],
        [-dot(s, eye), -dot(u, eye), dot(f, eye), 1.0],
    ];
    (m, light)
}

fn mul(m: [[f32; 4]; 4], v: [f32; 3]) -> [f32; 4] {
    mul4(m, [v[0], v[1], v[2], 1.0])
}

fn mul4(m: [[f32; 4]; 4], v: [f32; 4]) -> [f32; 4] {
    [
        m[0][0] * v[0] + m[1][0] * v[1] + m[2][0] * v[2] + m[3][0] * v[3],
        m[0][1] * v[0] + m[1][1] * v[1] + m[2][1] * v[2] + m[3][1] * v[3],
        m[0][2] * v[0] + m[1][2] * v[1] + m[2][2] * v[2] + m[3][2] * v[3],
        m[0][3] * v[0] + m[1][3] * v[1] + m[2][3] * v[2] + m[3][3] * v[3],
    ]
}

/// Clip a view-space triangle to `depth >= near`. Yields 0, 3 or 4 vertices
/// (a quad when one corner was cut off), to be drawn as a fan.
fn clip_near(tri: [[f32; 3]; 3], near: f32) -> ([[f32; 3]; 4], usize) {
    let mut out = [[0.0; 3]; 4];
    let mut n = 0;
    for i in 0..3 {
        let (a, b) = (tri[i], tri[(i + 1) % 3]);
        let (a_in, b_in) = (a[2] >= near, b[2] >= near);
        if a_in {
            out[n] = a;
            n += 1;
        }
        if a_in != b_in {
            let t = (near - a[2]) / (b[2] - a[2]);
            out[n] = [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, near];
            n += 1;
        }
    }
    (out, n)
}

#[allow(clippy::too_many_arguments)]
fn fill_triangle(
    color: &mut [[u8; 4]],
    zbuf: &mut [f32],
    w: i32,
    h: i32,
    a: [f32; 3],
    b: [f32; 3],
    c: [f32; 3],
    shade: [u8; 4],
) {
    let min_x = a[0].min(b[0]).min(c[0]).floor().max(0.0) as i32;
    let max_x = a[0].max(b[0]).max(c[0]).ceil().min(w as f32 - 1.0) as i32;
    let min_y = a[1].min(b[1]).min(c[1]).floor().max(0.0) as i32;
    let max_y = a[1].max(b[1]).max(c[1]).ceil().min(h as f32 - 1.0) as i32;
    if min_x > max_x || min_y > max_y {
        return;
    }
    let area = edge(a, b, c);
    if area.abs() < 1e-6 {
        return;
    }
    let inv_area = 1.0 / area;
    for y in min_y..=max_y {
        for x in min_x..=max_x {
            let p = [x as f32 + 0.5, y as f32 + 0.5, 0.0];
            let w0 = edge(b, c, p) * inv_area;
            let w1 = edge(c, a, p) * inv_area;
            let w2 = edge(a, b, p) * inv_area;
            if w0 < 0.0 || w1 < 0.0 || w2 < 0.0 {
                continue;
            }
            let inv_depth = w0 * a[2] + w1 * b[2] + w2 * c[2];
            let idx = (y * w + x) as usize;
            if inv_depth > zbuf[idx] {
                zbuf[idx] = inv_depth;
                color[idx] = shade;
            }
        }
    }
}

fn edge(a: [f32; 3], b: [f32; 3], c: [f32; 3]) -> f32 {
    (c[0] - a[0]) * (b[1] - a[1]) - (c[1] - a[1]) * (b[0] - a[0])
}

fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}
fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}
fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}
fn normalize(v: [f32; 3]) -> [f32; 3] {
    let l = dot(v, v).sqrt().max(1e-8);
    [v[0] / l, v[1] / l, v[2] / l]
}

#[cfg(test)]
mod tests {
    use super::*;
    use pam_core::Mesh;

    #[test]
    fn cube_is_not_empty_pixels() {
        let tris = [
            [[0., 0., 0.], [1., 0., 0.], [1., 1., 0.]],
            [[0., 0., 0.], [1., 1., 0.], [0., 1., 0.]],
            [[0., 0., 1.], [1., 1., 1.], [1., 0., 1.]],
            [[0., 0., 1.], [0., 1., 1.], [1., 1., 1.]],
            [[0., 0., 0.], [0., 0., 1.], [1., 0., 1.]],
            [[0., 0., 0.], [1., 0., 1.], [1., 0., 0.]],
            [[0., 1., 0.], [1., 1., 0.], [1., 1., 1.]],
            [[0., 1., 0.], [1., 1., 1.], [0., 1., 1.]],
            [[0., 0., 0.], [0., 1., 0.], [0., 1., 1.]],
            [[0., 0., 0.], [0., 1., 1.], [0., 0., 1.]],
            [[1., 0., 0.], [1., 0., 1.], [1., 1., 1.]],
            [[1., 0., 0.], [1., 1., 1.], [1., 1., 0.]],
        ];
        let mesh = Mesh::from_triangles(&tris);
        let img = render_thumbnail(&mesh, 64);
        let painted = img.pixels().filter(|p| p.0 != BG).count();
        assert!(
            painted > 80,
            "expected a visible cube, got {painted} pixels"
        );
    }

    fn unit_cube() -> Mesh {
        Mesh::from_triangles(&[
            [[0., 0., 0.], [1., 0., 0.], [1., 1., 0.]],
            [[0., 0., 0.], [1., 1., 0.], [0., 1., 0.]],
            [[0., 0., 1.], [1., 1., 1.], [1., 0., 1.]],
            [[0., 0., 1.], [0., 1., 1.], [1., 1., 1.]],
            [[0., 0., 0.], [0., 0., 1.], [1., 0., 1.]],
            [[0., 0., 0.], [1., 0., 1.], [1., 0., 0.]],
            [[0., 1., 0.], [1., 1., 0.], [1., 1., 1.]],
            [[0., 1., 0.], [1., 1., 1.], [0., 1., 1.]],
            [[0., 0., 0.], [0., 1., 0.], [0., 1., 1.]],
            [[0., 0., 0.], [0., 1., 1.], [0., 0., 1.]],
            [[1., 0., 0.], [1., 0., 1.], [1., 1., 1.]],
            [[1., 0., 0.], [1., 1., 1.], [1., 1., 0.]],
        ])
    }

    #[test]
    fn empty_mesh_fills_custom_background() {
        let mesh = Mesh {
            vertices: Vec::new(),
            indices: Vec::new(),
            bbox: pam_core::BBox::empty(),
        };
        let bg = [9, 8, 7, 255];
        let img = render_mesh(&mesh, &Camera::default(), 4, 4, bg);
        assert!(img.pixels().all(|p| p.0 == bg));
    }

    #[test]
    fn encode_png_writes_png_header() {
        let img = render_thumbnail(&unit_cube(), 16);
        let bytes = encode_png(&img);
        assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
    }

    #[test]
    fn camera_orbit_and_zoom_clamp() {
        let mut cam = Camera::isometric();
        cam.orbit(0.0, 10_000.0);
        assert!((cam.pitch - 1.45).abs() < 1e-5);
        cam.orbit(0.0, -10_000.0);
        assert!((cam.pitch + 1.45).abs() < 1e-5);
        cam.zoom(1_000_000.0);
        assert_eq!(cam.distance, 0.4);
        cam.zoom(-1_000_000.0);
        assert_eq!(cam.distance, 12.0);
    }

    #[test]
    fn orbiting_still_paints_the_cube() {
        let mut cam = Camera::default();
        cam.orbit(80.0, -20.0);
        let img = render_mesh(&unit_cube(), &cam, 48, 48, BG);
        let painted = img.pixels().filter(|p| p.0 != BG).count();
        assert!(
            painted > 40,
            "expected visible cube after orbit, got {painted}"
        );
    }

    #[test]
    fn clip_near_keeps_splits_or_drops() {
        let near = 1.0;
        let tri = |d: [f32; 3]| [[0., 0., d[0]], [1., 0., d[1]], [0., 1., d[2]]];
        // Entirely in front: unchanged.
        assert_eq!(clip_near(tri([2., 3., 4.]), near).1, 3);
        // Entirely behind the near plane (or the eye): nothing to draw.
        assert_eq!(clip_near(tri([0.5, -1., -2.]), near).1, 0);
        // One corner behind: a quad, all of it on or in front of the plane.
        let (quad, n) = clip_near(tri([-1., 3., 3.]), near);
        assert_eq!(n, 4);
        assert!(quad.iter().all(|v| v[2] >= near));
        // Two corners behind: a smaller triangle.
        let (small, n) = clip_near(tri([3., -1., -1.]), near);
        assert_eq!(n, 3);
        assert!(small[..3].iter().all(|v| v[2] >= near));
    }

    #[test]
    fn thin_plate_back_face_does_not_bleed_through() {
        // 2 m plate, 0.1 mm thick, seen from above: only the top face's shade
        // may appear, never the bottom face z-fighting through it.
        let (s, t) = (2000.0, 0.1);
        let plate = Mesh::from_triangles(&[
            [[0., t, 0.], [0., t, s], [s, t, s]],
            [[0., t, 0.], [s, t, s], [s, t, 0.]],
            [[0., 0., 0.], [s, 0., s], [0., 0., s]],
            [[0., 0., 0.], [s, 0., 0.], [s, 0., s]],
        ]);
        let img = render_mesh(&plate, &Camera::default(), 128, 128, BG);
        // The plate has no side walls, so a sliver of the bottom face is
        // legitimately visible past the front edge; judge the interior only.
        let shades: std::collections::HashSet<[u8; 4]> = img
            .enumerate_pixels()
            .filter(|(x, y, _)| (40..88).contains(x) && (40..88).contains(y))
            .map(|(_, _, p)| p.0)
            .filter(|p| *p != BG)
            .collect();
        assert_eq!(shades.len(), 1, "saw {} shades: {shades:?}", shades.len());
    }
}
