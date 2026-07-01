//! Run an unmodified static Linux binary in ring 0: load its ELF, build a
//! Linux-shaped process stack (argc/argv/envp/auxv), point the `syscall`
//! instruction at our handler via the LSTAR MSR, and jump to its entry.
//! Its syscalls get translated to our world: write goes to serial, brk and
//! mmap are bump allocators, exit_group becomes our exit port.

#![no_std]
#![no_main]

use core::arch::{asm, naked_asm};
use core::fmt::Write;
use core::sync::atomic::{AtomicU64, Ordering::Relaxed};

use kernel::{Serial, exit, outb};
use shared::{PROGRAM_ADDR, SCRATCH_ADDR, SERIAL_PORT};

// Memory layout for the hosted program (see shared/src/lib.rs).
const BRK_MAX: u64 = 0x00e0_0000;
const MMAP_BASE: u64 = 0x0280_0000;
const MMAP_MAX: u64 = 0x02f0_0000;
const PROGRAM_STACK_TOP: u64 = 0x0300_0000;

// The syscall stub spills registers to a fixed frame and runs the Rust
// handler on its own small stack, both in the scratch region.
const FRAME: u64 = SCRATCH_ADDR;
const SYSCALL_STACK_TOP: u64 = SCRATCH_ADDR + 0x11000;

const MSR_EFER: u32 = 0xc000_0080;
const MSR_STAR: u32 = 0xc000_0081;
const MSR_LSTAR: u32 = 0xc000_0082;
const MSR_SFMASK: u32 = 0xc000_0084;
const MSR_FS_BASE: u32 = 0xc000_0100;
const EFER_SCE: u64 = 1;

fn rdmsr(msr: u32) -> u64 {
    let (lo, hi): (u32, u32);
    unsafe { asm!("rdmsr", in("ecx") msr, out("eax") lo, out("edx") hi) };
    (hi as u64) << 32 | lo as u64
}

fn wrmsr(msr: u32, value: u64) {
    unsafe {
        asm!("wrmsr", in("ecx") msr, in("eax") value as u32, in("edx") (value >> 32) as u32);
    }
}

fn read<T: Copy>(addr: u64) -> T {
    unsafe { core::ptr::read_volatile(addr as *const T) }
}

fn write_val<T>(addr: u64, value: T) {
    unsafe { core::ptr::write_volatile(addr as *mut T, value) };
}

// ---------------------------------------------------------------- ELF loader

struct LoadedElf {
    entry: u64,
    phdr_vaddr: u64,
    phent: u64,
    phnum: u64,
    brk_start: u64,
}

/// Load the ET_EXEC image the VMM placed at PROGRAM_ADDR: copy PT_LOAD
/// segments to their link addresses, zero their .bss tails.
fn load_program() -> LoadedElf {
    let len: u64 = read(PROGRAM_ADDR);
    assert!(len != 0, "no program image; run: vmm <kernel> <static-linux-elf>");
    let image = PROGRAM_ADDR + 16;

    assert!(read::<u32>(image) == 0x464c_457f, "not an ELF");
    let e_type: u16 = read(image + 16);
    assert!(e_type == 2, "not ET_EXEC; build with -static -no-pie");
    let entry: u64 = read(image + 24);
    let phoff: u64 = read(image + 32);
    let phent: u16 = read(image + 54);
    let phnum: u16 = read(image + 56);

    let mut phdr_vaddr = 0u64;
    let mut brk_start = 0u64;
    for i in 0..phnum as u64 {
        let ph = image + phoff + i * phent as u64;
        let p_type: u32 = read(ph);
        let p_offset: u64 = read(ph + 8);
        let p_vaddr: u64 = read(ph + 16);
        let p_filesz: u64 = read(ph + 32);
        let p_memsz: u64 = read(ph + 40);
        if p_type == 6 {
            phdr_vaddr = p_vaddr; // PT_PHDR
        }
        if p_type != 1 {
            continue; // not PT_LOAD
        }
        assert!(p_vaddr + p_memsz <= BRK_MAX, "segment outside program region");
        unsafe {
            core::ptr::copy_nonoverlapping(
                (image + p_offset) as *const u8,
                p_vaddr as *mut u8,
                p_filesz as usize,
            );
            core::ptr::write_bytes((p_vaddr + p_filesz) as *mut u8, 0, (p_memsz - p_filesz) as usize);
        }
        if phdr_vaddr == 0 && phoff >= p_offset && phoff < p_offset + p_filesz {
            phdr_vaddr = p_vaddr + (phoff - p_offset);
        }
        brk_start = brk_start.max((p_vaddr + p_memsz + 0xfff) & !0xfff);
    }
    LoadedElf { entry, phdr_vaddr, phent: phent as u64, phnum: phnum as u64, brk_start }
}

/// Build the SysV process-entry stack: rsp points at argc, then argv,
/// envp and auxv follow, all NULL-terminated. rsp must be 16-aligned.
fn build_stack(elf: &LoadedElf) -> u64 {
    let argv0 = PROGRAM_STACK_TOP - 32;
    for (i, byte) in b"hello\0".iter().enumerate() {
        write_val(argv0 + i as u64, *byte);
    }
    let random = PROGRAM_STACK_TOP - 16; // 16 "random" bytes for AT_RANDOM
    write_val(random, 0x243f_6a88_85a3_08d3u64);
    write_val(random + 8, 0x1319_8a2e_0370_7344u64);

    let vector: [u64; 20] = [
        1,        // argc
        argv0, 0, // argv, NULL
        0,        // envp NULL
        3, elf.phdr_vaddr, // AT_PHDR
        4, elf.phent,      // AT_PHENT
        5, elf.phnum,      // AT_PHNUM
        6, 4096,           // AT_PAGESZ
        25, random,        // AT_RANDOM
        9, elf.entry,      // AT_ENTRY
        23, 0,             // AT_SECURE
        0, 0,              // AT_NULL
    ];
    let rsp = (PROGRAM_STACK_TOP - 64 - size_of_val(&vector) as u64) & !0xf;
    for (i, value) in vector.iter().enumerate() {
        write_val(rsp + i as u64 * 8, *value);
    }
    rsp
}

// ------------------------------------------------------------ syscall entry

/// `syscall` lands here (LSTAR). No stack switch happens in ring 0, so we
/// spill state to a fixed frame, hop onto our own stack, and return with a
/// plain jmp: sysret would force us to ring 3.
#[unsafe(naked)]
extern "C" fn syscall_entry() {
    naked_asm!(
        "mov [{f} + 0], rax",  // syscall number
        "mov [{f} + 8], rdi",
        "mov [{f} + 16], rsi",
        "mov [{f} + 24], rdx",
        "mov [{f} + 32], r10", // arg 4 lives in r10, not rcx
        "mov [{f} + 40], r8",
        "mov [{f} + 48], r9",
        "mov [{f} + 56], rcx", // return rip
        "mov [{f} + 64], r11", // return rflags
        "mov [{f} + 72], rsp", // program stack
        "mov rsp, {kstack}",
        "call {handler}",
        "mov rsp, [{f} + 72]",
        "mov rcx, [{f} + 56]",
        "push qword ptr [{f} + 64]",
        "popfq",
        "jmp rcx",
        f = const FRAME,
        kstack = const SYSCALL_STACK_TOP,
        handler = sym syscall_handler,
    )
}

static BRK: AtomicU64 = AtomicU64::new(0);
static MMAP: AtomicU64 = AtomicU64::new(MMAP_BASE);

const ENOTTY: u64 = -25i64 as u64;
const EBADF: u64 = -9i64 as u64;
const EINVAL: u64 = -22i64 as u64;
const ENOMEM: u64 = -12i64 as u64;
const ENOSYS: u64 = -38i64 as u64;

fn write_out(buf: u64, count: u64) {
    for i in 0..count {
        outb(SERIAL_PORT, read(buf + i));
    }
}

extern "C" fn syscall_handler() -> u64 {
    let nr: u64 = read(FRAME);
    let (a1, a2, a3): (u64, u64, u64) = (read(FRAME + 8), read(FRAME + 16), read(FRAME + 24));
    let a4: u64 = read(FRAME + 32);
    match nr {
        1 | 20 if a1 != 1 && a1 != 2 => EBADF,
        1 => {
            // write(fd, buf, count)
            write_out(a2, a3);
            a3
        }
        20 => {
            // writev(fd, iov, iovcnt)
            let mut total = 0u64;
            for i in 0..a3 {
                let base: u64 = read(a2 + i * 16);
                let len: u64 = read(a2 + i * 16 + 8);
                write_out(base, len);
                total += len;
            }
            total
        }
        16 => ENOTTY, // ioctl: stdout is not a tty (musl probes TIOCGWINSZ)
        12 => {
            // brk(addr)
            if a1 >= BRK.load(Relaxed) && a1 < BRK_MAX {
                BRK.store(a1, Relaxed);
            }
            BRK.load(Relaxed)
        }
        9 => {
            // mmap(addr, len, prot, flags, fd, off): anonymous only
            if a4 & 0x20 == 0 {
                return ENOSYS;
            }
            let len = (a2 + 0xfff) & !0xfff;
            let addr = MMAP.fetch_add(len, Relaxed);
            if addr + len > MMAP_MAX { ENOMEM } else { addr }
        }
        11 => 0, // munmap: the arena never shrinks
        158 => match a1 {
            0x1002 => {
                wrmsr(MSR_FS_BASE, a2); // arch_prctl(ARCH_SET_FS): TLS pointer
                0
            }
            0x1003 => {
                write_val(a2, rdmsr(MSR_FS_BASE));
                0
            }
            _ => EINVAL,
        },
        218 => 1,            // set_tid_address
        13 | 14 => 0,        // rt_sigaction / rt_sigprocmask: pretend
        39 | 186 => 1,       // getpid / gettid
        102 | 104 | 107 | 108 => 0, // getuid / getgid / geteuid / getegid
        60 | 231 => {
            // exit / exit_group
            let _ = writeln!(Serial, "[kernel] program exited with {}", a1 as i64);
            exit(a1 as u8);
        }
        _ => {
            let _ = writeln!(Serial, "[kernel] unhandled syscall {nr}");
            ENOSYS
        }
    }
}

// -------------------------------------------------------------------- start

#[unsafe(no_mangle)]
extern "C" fn _start() -> ! {
    let elf = load_program();
    BRK.store(elf.brk_start, Relaxed);
    let _ = writeln!(
        Serial,
        "[kernel] loaded ELF: entry {:#x}, {} phdrs, brk starts at {:#x}",
        elf.entry, elf.phnum, elf.brk_start
    );

    wrmsr(MSR_EFER, rdmsr(MSR_EFER) | EFER_SCE);
    wrmsr(MSR_STAR, 0x8 << 32); // syscall loads CS=0x8, SS=0x10
    wrmsr(MSR_LSTAR, syscall_entry as *const () as u64);
    wrmsr(MSR_SFMASK, 0x200); // mask IF on syscall entry
    let _ = writeln!(Serial, "[kernel] MSRs set: efer={:#x} lstar={:#x}", rdmsr(MSR_EFER), rdmsr(MSR_LSTAR));

    let rsp = build_stack(&elf);
    let _ = writeln!(Serial, "[kernel] jumping to program, rsp={rsp:#x}");
    unsafe {
        asm!(
            "xor edx, edx", // no atexit handler from the "dynamic linker"
            "xor ebp, ebp",
            "mov rsp, {rsp}",
            "jmp {entry}",
            rsp = in(reg) rsp,
            entry = in(reg) elf.entry,
            options(noreturn),
        )
    }
}
