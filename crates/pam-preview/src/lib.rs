use image::{Rgba, RgbaImage};
use pam_core::Mesh;

const MAX_PREVIEW_TRIS: usize = 500_000;
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
    render_mesh(mesh, &Camera::isometric(), size, size)
}

pub fn render_mesh(mesh: &Mesh, camera: &Camera, width: u32, height: u32) -> RgbaImage {
    let mesh = mesh.simplified(MAX_PREVIEW_TRIS);
    let w = width.max(1) as i32;
    let h = height.max(1) as i32;
    let mut color = vec![BG; (w * h) as usize];
    let mut zbuf = vec![f32::INFINITY; (w * h) as usize];

    if mesh.is_empty() || !mesh.bbox.is_valid() {
        return to_image(width, height, &color);
    }

    let center = mesh.bbox.center();
    let radius = mesh.bbox.diagonal().max(1e-3) * 0.5;
    let (view, light) = view_matrix(camera, center, radius);
    let aspect = width as f32 / height.max(1) as f32;
    let proj = perspective(40f32.to_radians(), aspect, 0.05, 100.0);

    let verts: Vec<[f32; 3]> = mesh
        .vertices
        .iter()
        .map(|v| project(proj, view, *v))
        .collect();

    let n_tris = mesh.triangle_count();
    for t in 0..n_tris {
        let i0 = mesh.indices[t * 3] as usize;
        let i1 = mesh.indices[t * 3 + 1] as usize;
        let i2 = mesh.indices[t * 3 + 2] as usize;
        let (a, b, c) = (verts[i0], verts[i1], verts[i2]);
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
        fill_triangle(
            &mut color,
            &mut zbuf,
            w,
            h,
            ndc_to_px(a, w, h),
            ndc_to_px(b, w, h),
            ndc_to_px(c, w, h),
            shade,
        );
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
    let mut img = RgbaImage::new(width, height);
    for y in 0..height {
        for x in 0..width {
            let c = color[(y * width + x) as usize];
            img.put_pixel(x, y, Rgba(c));
        }
    }
    img
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

fn perspective(fovy: f32, aspect: f32, near: f32, far: f32) -> [[f32; 4]; 4] {
    let f = 1.0 / (fovy * 0.5).tan();
    [
        [f / aspect, 0.0, 0.0, 0.0],
        [0.0, f, 0.0, 0.0],
        [0.0, 0.0, (far + near) / (near - far), -1.0],
        [0.0, 0.0, (2.0 * far * near) / (near - far), 0.0],
    ]
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

fn project(proj: [[f32; 4]; 4], view: [[f32; 4]; 4], v: [f32; 3]) -> [f32; 3] {
    let clip = mul4(proj, mul(view, v));
    let w = if clip[3].abs() < 1e-8 { 1e-8 } else { clip[3] };
    [clip[0] / w, clip[1] / w, clip[2] / w]
}

fn ndc_to_px(p: [f32; 3], w: i32, h: i32) -> [f32; 3] {
    [
        (p[0] * 0.5 + 0.5) * (w as f32 - 1.0),
        (1.0 - (p[1] * 0.5 + 0.5)) * (h as f32 - 1.0),
        p[2],
    ]
}

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
    for y in min_y..=max_y {
        for x in min_x..=max_x {
            let p = [x as f32 + 0.5, y as f32 + 0.5, 0.0];
            let w0 = edge(b, c, p) / area;
            let w1 = edge(c, a, p) / area;
            let w2 = edge(a, b, p) / area;
            if w0 < 0.0 || w1 < 0.0 || w2 < 0.0 {
                continue;
            }
            let z = w0 * a[2] + w1 * b[2] + w2 * c[2];
            let idx = (y * w + x) as usize;
            if z < zbuf[idx] {
                zbuf[idx] = z;
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
}
