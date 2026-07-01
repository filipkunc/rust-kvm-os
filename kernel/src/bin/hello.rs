//! The smallest guest: prove we're alive over the serial port, do one
//! hardware-float computation, and exit.

#![no_std]
#![no_main]

use core::fmt::Write;

use kernel::{Serial, exit};

#[unsafe(no_mangle)]
extern "C" fn _start() -> ! {
    let _ = writeln!(Serial, "hello from ring 0");
    let hypotenuse = libm::sqrtf(3.0f32 * 3.0 + 4.0 * 4.0);
    let _ = writeln!(Serial, "sqrt(3*3 + 4*4) = {hypotenuse}");
    exit(0)
}
