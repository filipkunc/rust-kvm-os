//! Ring-0 rasterizer: a flat-shaded torus with a z-buffer, drag to orbit,
//! wheel to zoom, space toggles the auto-spin.

#![no_std]
#![no_main]

use core::fmt::Write;
use core::ops::{Add, Mul, Sub};

use kernel::{Serial, fb, frame_done, read_state};
use libm::{cosf, sinf, sqrtf};
use shared::{
    ABS_X, ABS_Y, BTN_LEFT, EV_ABS, EV_KEY, EV_REL, FB_HEIGHT, FB_WIDTH, KEY_SPACE, REL_WHEEL,
    SCRATCH_ADDR,
};

const W: usize = FB_WIDTH;
const H: usize = FB_HEIGHT;
const FOCAL: f32 = H as f32; // ~53 degree vertical FOV

// Torus tessellation: SEG_U around the ring, SEG_V around the tube.
const SEG_U: usize = 48;
const SEG_V: usize = 24;
const VERTS: usize = SEG_U * SEG_V;
const MAJOR_R: f32 = 1.15;
const MINOR_R: f32 = 0.5;

#[derive(Clone, Copy, Default)]
struct V3 {
    x: f32,
    y: f32,
    z: f32,
}

impl Add for V3 {
    type Output = V3;
    fn add(self, o: V3) -> V3 {
        V3 { x: self.x + o.x, y: self.y + o.y, z: self.z + o.z }
    }
}

impl Sub for V3 {
    type Output = V3;
    fn sub(self, o: V3) -> V3 {
        V3 { x: self.x - o.x, y: self.y - o.y, z: self.z - o.z }
    }
}

impl Mul<f32> for V3 {
    type Output = V3;
    fn mul(self, s: f32) -> V3 {
        V3 { x: self.x * s, y: self.y * s, z: self.z * s }
    }
}

impl V3 {
    fn dot(self, o: V3) -> f32 {
        self.x * o.x + self.y * o.y + self.z * o.z
    }

    fn normalized(self) -> V3 {
        self * (1.0 / sqrtf(self.dot(self)))
    }

    /// Yaw around Y, then pitch around X.
    fn rotated(self, yaw: f32, pitch: f32) -> V3 {
        let (sy, cy) = (sinf(yaw), cosf(yaw));
        let (sp, cp) = (sinf(pitch), cosf(pitch));
        let x = self.x * cy + self.z * sy;
        let z = -self.x * sy + self.z * cy;
        V3 { x, y: self.y * cp - z * sp, z: self.y * sp + z * cp }
    }
}

/// Screen-space vertex: x, y in pixels, plus 1/z for the depth buffer
/// (1/z interpolates linearly across the screen; z does not).
#[derive(Clone, Copy, Default)]
struct Screen {
    x: f32,
    y: f32,
    zinv: f32,
    behind: bool,
}

fn zbuf() -> &'static mut [f32] {
    unsafe { core::slice::from_raw_parts_mut(SCRATCH_ADDR as *mut f32, W * H) }
}

fn rgb(r: f32, g: f32, b: f32) -> u32 {
    let to8 = |v: f32| (v.clamp(0.0, 1.0) * 255.0) as u32;
    to8(b) | (to8(g) << 8) | (to8(r) << 16)
}

fn torus_point(i: usize, j: usize) -> (V3, V3) {
    let u = i as f32 * (core::f32::consts::TAU / SEG_U as f32);
    let v = j as f32 * (core::f32::consts::TAU / SEG_V as f32);
    let (cu, su) = (cosf(u), sinf(u));
    let (cv, sv) = (cosf(v), sinf(v));
    let position = V3 {
        x: (MAJOR_R + MINOR_R * cv) * cu,
        y: MINOR_R * sv,
        z: (MAJOR_R + MINOR_R * cv) * su,
    };
    let normal = V3 { x: cv * cu, y: sv, z: cv * su };
    (position, normal)
}

fn edge(a: Screen, b: Screen, px: f32, py: f32) -> f32 {
    (px - a.x) * (b.y - a.y) - (py - a.y) * (b.x - a.x)
}

fn fill_triangle(fb: &mut [u32], zb: &mut [f32], v: [Screen; 3], color: u32) {
    if v[0].behind || v[1].behind || v[2].behind {
        return; // no near-plane clipping; the camera zoom is clamped instead
    }
    let area2 = edge(v[0], v[1], v[2].x, v[2].y);
    if area2 <= 0.0 {
        return; // backface (or degenerate)
    }
    let min_x = v.iter().fold(f32::MAX, |m, p| m.min(p.x)).max(0.0) as usize;
    let min_y = v.iter().fold(f32::MAX, |m, p| m.min(p.y)).max(0.0) as usize;
    let max_x = (v.iter().fold(0.0f32, |m, p| m.max(p.x)) as usize).min(W - 1);
    let max_y = (v.iter().fold(0.0f32, |m, p| m.max(p.y)) as usize).min(H - 1);
    let inv_area = 1.0 / area2;
    for y in min_y..=max_y {
        let py = y as f32 + 0.5;
        for x in min_x..=max_x {
            let px = x as f32 + 0.5;
            let w0 = edge(v[1], v[2], px, py);
            let w1 = edge(v[2], v[0], px, py);
            let w2 = edge(v[0], v[1], px, py);
            if w0 < 0.0 || w1 < 0.0 || w2 < 0.0 {
                continue;
            }
            let zinv = (w0 * v[0].zinv + w1 * v[1].zinv + w2 * v[2].zinv) * inv_area;
            let idx = y * W + x;
            if zinv > zb[idx] {
                zb[idx] = zinv;
                fb[idx] = color;
            }
        }
    }
}

fn clear(fb: &mut [u32], zb: &mut [f32]) {
    for y in 0..H {
        let v = 0.05 + 0.05 * (y as f32 / H as f32);
        let color = rgb(v * 0.6, v * 0.7, v + 0.02);
        fb[y * W..(y + 1) * W].fill(color);
    }
    zb.fill(0.0);
}

#[unsafe(no_mangle)]
extern "C" fn _start() -> ! {
    let _ = writeln!(Serial, "torus kernel up; drag orbits, wheel zooms, space toggles spin");

    let light = V3 { x: -0.45, y: 0.8, z: -0.4 }.normalized();
    let mut mouse = (0i32, 0i32);
    let mut down = false;
    let mut spin = true;
    let mut yaw = 0.0f32;
    let mut pitch = 0.4f32;
    let mut dist = 4.0f32;
    let mut last_time = 0.0f32;

    let mut view_normals = [V3::default(); VERTS];
    let mut projected = [Screen::default(); VERTS];

    loop {
        let state = read_state();
        let time = state.time_ns as f32 / 1e9;
        let dt = (time - last_time).clamp(0.0, 0.1);
        last_time = time;

        for event in &state.events[..state.event_count.min(shared::MAX_EVENTS as u32) as usize] {
            match (event.kind, event.code) {
                (EV_ABS, ABS_X) => {
                    let x = event.value as i32;
                    if down {
                        yaw += (x - mouse.0) as f32 * 0.01;
                    }
                    mouse.0 = x;
                }
                (EV_ABS, ABS_Y) => {
                    let y = event.value as i32;
                    if down {
                        pitch = (pitch + (y - mouse.1) as f32 * 0.01).clamp(-1.5, 1.5);
                    }
                    mouse.1 = y;
                }
                (EV_KEY, BTN_LEFT) => down = event.value == 1,
                (EV_REL, REL_WHEEL) => {
                    dist = (dist - event.value as i32 as f32 * 0.4).clamp(2.4, 10.0);
                }
                (EV_KEY, KEY_SPACE) if event.value == 1 => spin = !spin,
                _ => {}
            }
        }
        if spin {
            yaw += dt * 0.5;
        }

        for i in 0..SEG_U {
            for j in 0..SEG_V {
                let (position, normal) = torus_point(i, j);
                let p = position.rotated(yaw, pitch);
                let z = p.z + dist;
                view_normals[i * SEG_V + j] = normal.rotated(yaw, pitch);
                projected[i * SEG_V + j] = Screen {
                    x: W as f32 * 0.5 + FOCAL * p.x / z,
                    y: H as f32 * 0.5 - FOCAL * p.y / z,
                    zinv: 1.0 / z,
                    behind: z < 0.1,
                };
            }
        }

        let (fb, zb) = (fb(), zbuf());
        clear(fb, zb);
        for i in 0..SEG_U {
            for j in 0..SEG_V {
                let quad = [
                    i * SEG_V + j,
                    ((i + 1) % SEG_U) * SEG_V + j,
                    i * SEG_V + (j + 1) % SEG_V,
                    ((i + 1) % SEG_U) * SEG_V + (j + 1) % SEG_V,
                ];
                let normal = (view_normals[quad[0]]
                    + view_normals[quad[1]]
                    + view_normals[quad[2]]
                    + view_normals[quad[3]])
                .normalized();
                let diffuse = normal.dot(light).max(0.0);
                let shade = 0.12 + 0.88 * diffuse;
                let color = if (i / 4 + j / 4) % 2 == 0 {
                    rgb(shade * 0.95, shade * 0.5, shade * 0.2)
                } else {
                    rgb(shade * 0.85, shade * 0.82, shade * 0.78)
                };
                // This order makes outward faces come out with positive
                // signed area after projection (the screen y-flip mirrors
                // orientation once; the u,v surface basis mirrors it back).
                fill_triangle(fb, zb, [projected[quad[0]], projected[quad[1]], projected[quad[2]]], color);
                fill_triangle(fb, zb, [projected[quad[2]], projected[quad[1]], projected[quad[3]]], color);
            }
        }

        frame_done();
    }
}
