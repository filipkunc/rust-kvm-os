#![no_std]
#![no_main]

use core::fmt::Write;

use kernel::{Serial, fb, frame_done, read_state};
use shared::{
    ABS_X, ABS_Y, BTN_LEFT, EV_ABS, EV_KEY, FB_HEIGHT, FB_WIDTH, KEY_C, KEY_SPACE, SCRATCH_ADDR,
};

const W: i32 = FB_WIDTH as i32;
const H: i32 = FB_HEIGHT as i32;

fn canvas() -> &'static mut [u32] {
    unsafe { core::slice::from_raw_parts_mut(SCRATCH_ADDR as *mut u32, FB_WIDTH * FB_HEIGHT) }
}

fn rgb(r: f32, g: f32, b: f32) -> u32 {
    let to8 = |v: f32| (v.clamp(0.0, 1.0) * 255.0) as u32;
    to8(b) | (to8(g) << 8) | (to8(r) << 16)
}

/// Hue in [0, 6) to fully saturated RGB.
fn hue_to_rgb(h: f32) -> u32 {
    let f = h - libm::floorf(h / 6.0) * 6.0;
    let x = 1.0 - libm::fabsf(f - libm::floorf(f / 2.0) * 2.0 - 1.0);
    match f as i32 {
        0 => rgb(1.0, x, 0.0),
        1 => rgb(x, 1.0, 0.0),
        2 => rgb(0.0, 1.0, x),
        3 => rgb(0.0, x, 1.0),
        4 => rgb(x, 0.0, 1.0),
        _ => rgb(1.0, 0.0, x),
    }
}

/// Dark blue vignette so the first frame already shows per-pixel float math.
fn paint_background(dst: &mut [u32]) {
    for y in 0..H {
        for x in 0..W {
            let dx = (x - W / 2) as f32 / W as f32;
            let dy = (y - H / 2) as f32 / H as f32;
            let d = libm::sqrtf(dx * dx + dy * dy);
            let v = 0.25 - d * 0.22;
            dst[(y * W + x) as usize] = rgb(v * 0.4, v * 0.5, v + 0.06);
        }
    }
}

fn stamp(dst: &mut [u32], cx: i32, cy: i32, radius: i32, color: u32) {
    for y in (cy - radius).max(0)..(cy + radius + 1).min(H) {
        for x in (cx - radius).max(0)..(cx + radius + 1).min(W) {
            let (dx, dy) = (x - cx, y - cy);
            if dx * dx + dy * dy <= radius * radius {
                dst[(y * W + x) as usize] = color;
            }
        }
    }
}

fn stroke(dst: &mut [u32], from: (i32, i32), to: (i32, i32), color: u32) {
    let (dx, dy) = (to.0 - from.0, to.1 - from.1);
    let steps = dx.abs().max(dy.abs()).max(1);
    for i in 0..=steps {
        stamp(dst, from.0 + dx * i / steps, from.1 + dy * i / steps, 6, color);
    }
}

fn crosshair(dst: &mut [u32], x: i32, y: i32, color: u32) {
    for d in -8..=8i32 {
        if d.abs() > 2 {
            let (px, py) = (x + d, y);
            if (0..W).contains(&px) && (0..H).contains(&py) {
                dst[(py * W + px) as usize] = color;
            }
            let (px, py) = (x, y + d);
            if (0..W).contains(&px) && (0..H).contains(&py) {
                dst[(py * W + px) as usize] = color;
            }
        }
    }
}

#[unsafe(no_mangle)]
extern "C" fn _start() -> ! {
    let _ = writeln!(Serial, "paint kernel up; C cycles color, space clears");
    paint_background(canvas());

    let mut mouse = (W / 2, H / 2);
    let mut prev = mouse;
    let mut down = false;
    let mut hue = 0.6f32;

    loop {
        let state = read_state();
        for event in &state.events[..state.event_count.min(shared::MAX_EVENTS as u32) as usize] {
            match (event.kind, event.code) {
                (EV_ABS, ABS_X) => mouse.0 = (event.value as i32).clamp(0, W - 1),
                (EV_ABS, ABS_Y) => mouse.1 = (event.value as i32).clamp(0, H - 1),
                (EV_KEY, BTN_LEFT) => {
                    down = event.value == 1;
                    if down {
                        prev = mouse; // don't connect separate strokes
                    }
                }
                (EV_KEY, KEY_C) if event.value == 1 => hue += 1.3,
                (EV_KEY, KEY_SPACE) if event.value == 1 => paint_background(canvas()),
                _ => {}
            }
        }

        let brush = hue_to_rgb(hue);
        if down {
            stroke(canvas(), prev, mouse, brush);
        }
        prev = mouse;

        fb().copy_from_slice(canvas());
        let t = state.time_ns as f32 / 1e9;
        let pulse = 0.7 + 0.3 * libm::sinf(t * 5.0);
        crosshair(fb(), mouse.0, mouse.1, rgb(pulse, pulse, pulse));

        frame_done();
    }
}
