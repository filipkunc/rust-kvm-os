#![no_std]
#![no_main]

use core::arch::asm;
use core::fmt::Write;

const SERIAL_PORT: u16 = 0x3f8;
const EXIT_PORT: u16 = 0xf4;

fn outb(port: u16, value: u8) {
    unsafe {
        asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack, preserves_flags));
    }
}

struct Serial;

impl Write for Serial {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for byte in s.bytes() {
            outb(SERIAL_PORT, byte);
        }
        Ok(())
    }
}

fn exit(code: u8) -> ! {
    outb(EXIT_PORT, code);
    loop {
        unsafe { asm!("hlt") };
    }
}

#[unsafe(no_mangle)]
extern "C" fn _start() -> ! {
    let _ = writeln!(Serial, "hello from ring 0");

    // Hardware SSE proof: these lower to mulss/addss and libm's sqrtf runs
    // on real float registers. Under the stock soft-float target this whole
    // expression would be compiler-builtins libcalls.
    let hypotenuse = libm::sqrtf(3.0f32 * 3.0 + 4.0 * 4.0);
    let _ = writeln!(Serial, "sqrt(3*3 + 4*4) = {hypotenuse}");

    exit(0)
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    let _ = writeln!(Serial, "panic: {info}");
    exit(1)
}
