//! Common guest-side runtime: serial, exit, framebuffer, mailbox.

#![no_std]

use core::arch::asm;
use core::fmt::Write;

use shared::{EXIT_PORT, FB_ADDR, FB_HEIGHT, FB_WIDTH, FRAME_PORT, SERIAL_PORT, STATE_ADDR, SharedState};

pub fn outb(port: u16, value: u8) {
    unsafe {
        asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack, preserves_flags));
    }
}

pub struct Serial;

impl Write for Serial {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for byte in s.bytes() {
            outb(SERIAL_PORT, byte);
        }
        Ok(())
    }
}

pub fn exit(code: u8) -> ! {
    outb(EXIT_PORT, code);
    loop {
        unsafe { asm!("hlt") };
    }
}

pub fn fb() -> &'static mut [u32] {
    unsafe { core::slice::from_raw_parts_mut(FB_ADDR as *mut u32, FB_WIDTH * FB_HEIGHT) }
}

pub fn read_state() -> SharedState {
    unsafe { core::ptr::read_volatile(STATE_ADDR as *const SharedState) }
}

/// Signal end-of-frame and park until the VMM has blitted the framebuffer
/// and refilled the mailbox.
pub fn frame_done() {
    outb(FRAME_PORT, 0);
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    let _ = writeln!(Serial, "panic: {info}");
    exit(1)
}
