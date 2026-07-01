//! The guest/host ABI: one crate both sides depend on.
//!
//! Guest physical memory map (64 MiB RAM, identity-mapped):
//!
//! ```text
//! 0x0000_1000  page tables (3 pages, written by the VMM)
//! 0x001f_0000  stack top (grows down)
//! 0x0020_0000  kernel image (ELF load address)
//! 0x00f0_0000  SharedState mailbox (host writes while guest is parked)
//! 0x0100_0000  framebuffer, 640x480 0RGB u32 (guest writes, host blits)
//! 0x0180_0000  guest scratch (paint canvas)
//! ```

#![no_std]

pub const SERIAL_PORT: u16 = 0x3f8;
pub const EXIT_PORT: u16 = 0xf4;
/// Guest writes here when a frame is finished; the VMM parks the vCPU
/// until the frame is on screen and fresh input is in the mailbox.
pub const FRAME_PORT: u16 = 0xf5;

pub const FB_ADDR: u64 = 0x0100_0000;
pub const FB_WIDTH: usize = 640;
pub const FB_HEIGHT: usize = 480;

/// Demo-owned scratch region (paint canvas, z-buffer, ...).
pub const SCRATCH_ADDR: u64 = 0x0180_0000;

pub const STATE_ADDR: u64 = 0x00f0_0000;
pub const MAX_EVENTS: usize = 256;

// Input events use the evdev/virtio-input shape: {type, code, value}.
// Codes are real Linux evdev codes (winit hands them to the VMM as-is).
pub const EV_KEY: u16 = 0x01;
pub const EV_REL: u16 = 0x02;
pub const EV_ABS: u16 = 0x03;
pub const ABS_X: u16 = 0x00;
pub const ABS_Y: u16 = 0x01;
/// Wheel steps; the value is an i32 stored in the u32.
pub const REL_WHEEL: u16 = 0x08;
pub const BTN_LEFT: u16 = 0x110;
pub const KEY_SPACE: u16 = 57;
pub const KEY_C: u16 = 46;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct InputEvent {
    pub kind: u16,
    pub code: u16,
    pub value: u32,
}

/// Per-frame mailbox. The host fills it between the guest's FRAME_PORT
/// doorbell and the next KVM_RUN, so there is never concurrent access:
/// this is a mailbox, not a lock-free ring.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SharedState {
    pub frame: u64,
    /// Monotonic nanoseconds since the VMM started.
    pub time_ns: u64,
    pub event_count: u32,
    pub _pad: u32,
    pub events: [InputEvent; MAX_EVENTS],
}
