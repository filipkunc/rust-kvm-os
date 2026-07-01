use std::io::Write;

use kvm_bindings::{kvm_segment, kvm_userspace_memory_region};
use kvm_ioctls::{Kvm, VcpuExit};
use object::{Object, ObjectSegment};

const MEM_SIZE: usize = 64 << 20;

// Page tables for a 1 GiB identity map: one page each, placed below the kernel.
const PML4_ADDR: u64 = 0x1000;
const PDPT_ADDR: u64 = 0x2000;
const PD_ADDR: u64 = 0x3000;

// Grows down, well above the page tables and below the kernel at 0x200000.
const STACK_TOP: u64 = 0x1f0000;

const SERIAL_PORT: u16 = 0x3f8;
const EXIT_PORT: u16 = 0xf4;

// Control register bits (Intel SDM names).
const CR0_PE: u64 = 1 << 0;
const CR0_MP: u64 = 1 << 1;
const CR0_ET: u64 = 1 << 4;
const CR0_NE: u64 = 1 << 5;
const CR0_PG: u64 = 1 << 31;
const CR4_PAE: u64 = 1 << 5;
const CR4_OSFXSR: u64 = 1 << 9;
const CR4_OSXMMEXCPT: u64 = 1 << 10;
const EFER_LME: u64 = 1 << 8;
const EFER_LMA: u64 = 1 << 10;

// Page table entry bits.
const PTE_PRESENT: u64 = 1 << 0;
const PTE_WRITABLE: u64 = 1 << 1;
const PTE_HUGE: u64 = 1 << 7;

struct GuestMemory {
    ptr: *mut u8,
    size: usize,
}

impl GuestMemory {
    fn new(size: usize) -> Self {
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        assert!(ptr != libc::MAP_FAILED, "mmap of guest memory failed");
        Self { ptr: ptr.cast(), size }
    }

    fn write_bytes(&self, gpa: u64, bytes: &[u8]) {
        let end = gpa as usize + bytes.len();
        assert!(end <= self.size, "write past end of guest memory: {gpa:#x}+{:#x}", bytes.len());
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.ptr.add(gpa as usize), bytes.len());
        }
    }

    fn write_u64(&self, gpa: u64, value: u64) {
        self.write_bytes(gpa, &value.to_le_bytes());
    }
}

/// Identity-map the first 1 GiB with 2 MiB huge pages: PML4 -> PDPT -> PD.
fn write_page_tables(mem: &GuestMemory) {
    mem.write_u64(PML4_ADDR, PDPT_ADDR | PTE_PRESENT | PTE_WRITABLE);
    mem.write_u64(PDPT_ADDR, PD_ADDR | PTE_PRESENT | PTE_WRITABLE);
    for i in 0..512u64 {
        mem.write_u64(PD_ADDR + i * 8, (i << 21) | PTE_PRESENT | PTE_WRITABLE | PTE_HUGE);
    }
}

/// Load PT_LOAD segments of the kernel ELF into guest memory, return the entry point.
fn load_kernel(mem: &GuestMemory, path: &str) -> u64 {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("reading {path}: {e}"));
    let elf = object::File::parse(&*bytes).expect("parsing kernel ELF");
    for segment in elf.segments() {
        let data = segment.data().expect("reading ELF segment");
        if !data.is_empty() {
            mem.write_bytes(segment.address(), data);
        }
        // .bss (p_memsz > p_filesz) needs no zeroing: fresh anonymous mmap is zero-filled.
    }
    elf.entry()
}

fn code_segment() -> kvm_segment {
    kvm_segment {
        base: 0,
        limit: 0xfffff,
        selector: 0x8,
        type_: 0xb, // execute/read, accessed
        present: 1,
        dpl: 0,
        db: 0,
        s: 1, // code/data
        l: 1, // 64-bit
        g: 1,
        ..Default::default()
    }
}

fn data_segment() -> kvm_segment {
    kvm_segment {
        base: 0,
        limit: 0xfffff,
        selector: 0x10,
        type_: 0x3, // read/write, accessed
        present: 1,
        dpl: 0,
        db: 1,
        s: 1,
        l: 0,
        g: 1,
        ..Default::default()
    }
}

fn task_segment() -> kvm_segment {
    // VMX guest-state checks require TR to be a busy 64-bit TSS in long mode.
    kvm_segment {
        base: 0,
        limit: 0xfffff,
        selector: 0x18,
        type_: 0xb, // busy 64-bit TSS
        present: 1,
        dpl: 0,
        db: 0,
        s: 0, // system
        l: 0,
        g: 1,
        ..Default::default()
    }
}

fn main() {
    let kernel_path = std::env::args().nth(1).expect("usage: vmm <kernel-elf>");

    let kvm = Kvm::new().expect("opening /dev/kvm");
    let vm = kvm.create_vm().expect("creating VM");

    let mem = GuestMemory::new(MEM_SIZE);
    let region = kvm_userspace_memory_region {
        slot: 0,
        flags: 0,
        guest_phys_addr: 0,
        memory_size: MEM_SIZE as u64,
        userspace_addr: mem.ptr as u64,
    };
    // Safety: the mapping is valid, page-aligned, and outlives the VM.
    unsafe { vm.set_user_memory_region(region) }.expect("registering guest memory");

    write_page_tables(&mem);
    let entry = load_kernel(&mem, &kernel_path);

    let mut vcpu = vm.create_vcpu(0).expect("creating vCPU");

    // The whole "boot process": start the vCPU already in 64-bit long mode
    // with paging and SSE enabled. There is no firmware and no bootloader;
    // this struct is everything the guest inherits.
    let mut sregs = vcpu.get_sregs().expect("get_sregs");
    sregs.cr3 = PML4_ADDR;
    sregs.cr4 = CR4_PAE | CR4_OSFXSR | CR4_OSXMMEXCPT;
    sregs.cr0 = CR0_PE | CR0_MP | CR0_ET | CR0_NE | CR0_PG;
    sregs.efer = EFER_LME | EFER_LMA;
    sregs.cs = code_segment();
    let data = data_segment();
    sregs.ds = data;
    sregs.es = data;
    sregs.fs = data;
    sregs.gs = data;
    sregs.ss = data;
    sregs.tr = task_segment();
    vcpu.set_sregs(&sregs).expect("set_sregs");

    let mut regs = vcpu.get_regs().expect("get_regs");
    regs.rip = entry;
    regs.rsp = STACK_TOP;
    regs.rflags = 0x2; // bit 1 is reserved-must-be-one; interrupts stay off
    vcpu.set_regs(&regs).expect("set_regs");

    let mut stdout = std::io::stdout();
    loop {
        match vcpu.run().expect("KVM_RUN") {
            VcpuExit::IoOut(SERIAL_PORT, data) => {
                stdout.write_all(data).unwrap();
                stdout.flush().unwrap();
            }
            VcpuExit::IoOut(EXIT_PORT, data) => {
                let code = data[0];
                println!("[vmm] guest requested exit with code {code}");
                std::process::exit(code as i32);
            }
            VcpuExit::Hlt => {
                println!("[vmm] guest halted");
                break;
            }
            VcpuExit::Shutdown => {
                // With no IDT in the guest, any exception escalates to a
                // triple fault and lands here. The triple fault resets the
                // vCPU before we see the exit, so registers show reset state
                // (rip=0xfff0), not the crash site; recovering the faulting
                // state needs KVM_CAP_X86_TRIPLE_FAULT_EVENT.
                let regs = vcpu.get_regs().expect("get_regs");
                eprintln!("[vmm] guest crashed (triple fault); post-reset rip={:#x}", regs.rip);
                std::process::exit(1);
            }
            exit => {
                eprintln!("[vmm] unexpected exit: {exit:?}");
                std::process::exit(1);
            }
        }
    }
}
