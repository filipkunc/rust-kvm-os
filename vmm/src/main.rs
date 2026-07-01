use std::io::Write;
use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use kvm_bindings::{kvm_segment, kvm_userspace_memory_region};
use kvm_ioctls::{Kvm, VcpuExit, VcpuFd};
use object::{Object, ObjectSegment};
use shared::{
    ABS_X, ABS_Y, BTN_LEFT, EV_ABS, EV_KEY, EV_REL, EXIT_PORT, FB_ADDR, FB_HEIGHT, FB_WIDTH,
    FRAME_PORT, InputEvent, MAX_EVENTS, REL_WHEEL, SERIAL_PORT, STATE_ADDR, SharedState,
};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::platform::scancode::PhysicalKeyExtScancode;
use winit::window::{Window, WindowId};

const MEM_SIZE: usize = 64 << 20;

// Page tables for a 1 GiB identity map: one page each, placed below the kernel.
const PML4_ADDR: u64 = 0x1000;
const PDPT_ADDR: u64 = 0x2000;
const PD_ADDR: u64 = 0x3000;

// Grows down, well above the page tables and below the kernel at 0x200000.
const STACK_TOP: u64 = 0x1f0000;

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

    /// Copy one framebuffer row out of guest memory. The guest is parked at
    /// the doorbell while we blit, so this is not racing it; raw pointers
    /// (never references into guest RAM) keep it honest anyway.
    fn read_fb_row(&self, y: usize, out: &mut [u32]) {
        let src = unsafe { self.ptr.add(FB_ADDR as usize + y * FB_WIDTH * 4) as *const u32 };
        for (x, px) in out.iter_mut().enumerate().take(FB_WIDTH) {
            *px = unsafe { src.add(x).read_volatile() };
        }
    }

    fn write_state(&self, state: &SharedState) {
        unsafe {
            (self.ptr.add(STATE_ADDR as usize) as *mut SharedState).write_volatile(*state);
        }
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

fn setup_vcpu(vcpu: &mut VcpuFd, entry: u64) {
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
    // The SysV ABI puts rsp at 8 mod 16 on function entry (the call pushed a
    // return address). _start is compiled as a normal function, so entering
    // with a 16-aligned rsp makes every movaps spill misaligned: #GP, and
    // with no IDT, a triple fault.
    regs.rsp = STACK_TOP - 8;
    regs.rflags = 0x2; // bit 1 is reserved-must-be-one; interrupts stay off
    vcpu.set_regs(&regs).expect("set_regs");
}

/// Doorbell handshake: the vCPU thread parks here after the guest signals
/// end-of-frame; the event loop releases it after blitting the framebuffer
/// and refilling the input mailbox.
struct Gate {
    parked: Mutex<bool>,
    released: Condvar,
}

fn run_vcpu(mut vcpu: VcpuFd, gate: Arc<Gate>, proxy: winit::event_loop::EventLoopProxy<()>) {
    let mut stdout = std::io::stdout();
    loop {
        match vcpu.run().expect("KVM_RUN") {
            VcpuExit::IoOut(SERIAL_PORT, data) => {
                stdout.write_all(data).unwrap();
                stdout.flush().unwrap();
            }
            VcpuExit::IoOut(FRAME_PORT, _) => {
                let mut parked = gate.parked.lock().unwrap();
                *parked = true;
                if proxy.send_event(()).is_err() {
                    return; // event loop is gone, we are shutting down
                }
                while *parked {
                    parked = gate.released.wait(parked).unwrap();
                }
            }
            VcpuExit::IoOut(EXIT_PORT, data) => {
                let code = data[0];
                println!("[vmm] guest requested exit with code {code}");
                std::process::exit(code as i32);
            }
            VcpuExit::Hlt => {
                println!("[vmm] guest halted");
                std::process::exit(0);
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

struct App {
    mem: GuestMemory,
    gate: Arc<Gate>,
    window: Option<Rc<Window>>,
    surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
    size: PhysicalSize<u32>,
    pending: Vec<InputEvent>,
    start: Instant,
    frame: u64,
    redraw_requested_early: bool,
    dump_after: Option<u64>,
}

impl App {
    fn push(&mut self, kind: u16, code: u16, value: u32) {
        if self.pending.len() < MAX_EVENTS {
            self.pending.push(InputEvent { kind, code, value });
        }
    }

    /// Blit the guest framebuffer into the window, letterboxing if the
    /// compositor gave us a different size than we asked for.
    fn blit(&mut self) {
        let (Some(window), Some(surface)) = (&self.window, &mut self.surface) else {
            return;
        };
        let (w, h) = (self.size.width.max(1), self.size.height.max(1));
        surface
            .resize(NonZeroU32::new(w).unwrap(), NonZeroU32::new(h).unwrap())
            .expect("surface resize");
        let mut buffer = surface.buffer_mut().expect("surface buffer");
        buffer.fill(0);
        let mut row = [0u32; FB_WIDTH];
        for y in 0..FB_HEIGHT.min(h as usize) {
            self.mem.read_fb_row(y, &mut row);
            let dst = y * w as usize;
            let copy = FB_WIDTH.min(w as usize);
            buffer[dst..dst + copy].copy_from_slice(&row[..copy]);
        }
        window.pre_present_notify();
        buffer.present().expect("present");
    }

    /// If the guest is parked at the doorbell, hand it fresh input and time,
    /// then let it run the next frame.
    fn release_guest(&mut self) {
        let mut parked = self.gate.parked.lock().unwrap();
        if !*parked {
            return;
        }
        self.frame += 1;
        let mut state = SharedState {
            frame: self.frame,
            time_ns: self.start.elapsed().as_nanos() as u64,
            event_count: self.pending.len() as u32,
            _pad: 0,
            events: [InputEvent { kind: 0, code: 0, value: 0 }; MAX_EVENTS],
        };
        state.events[..self.pending.len()].copy_from_slice(&self.pending);
        self.pending.clear();
        self.mem.write_state(&state);
        *parked = false;
        self.gate.released.notify_all();
    }

    fn dump_ppm(&self, path: &str) {
        let mut out = Vec::with_capacity(FB_WIDTH * FB_HEIGHT * 3 + 32);
        out.extend_from_slice(format!("P6\n{FB_WIDTH} {FB_HEIGHT}\n255\n").as_bytes());
        let mut row = [0u32; FB_WIDTH];
        for y in 0..FB_HEIGHT {
            self.mem.read_fb_row(y, &mut row);
            for px in row {
                out.extend_from_slice(&[(px >> 16) as u8, (px >> 8) as u8, px as u8]);
            }
        }
        std::fs::write(path, out).expect("writing framebuffer dump");
        println!("[vmm] dumped frame {} to {path}", self.frame);
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let attrs = Window::default_attributes()
            .with_title("rust-kvm-os")
            .with_inner_size(PhysicalSize::new(FB_WIDTH as u32, FB_HEIGHT as u32))
            .with_resizable(false);
        let window = Rc::new(event_loop.create_window(attrs).expect("creating window"));
        self.size = window.inner_size();
        let context = softbuffer::Context::new(window.clone()).expect("softbuffer context");
        self.surface =
            Some(softbuffer::Surface::new(&context, window.clone()).expect("softbuffer surface"));
        self.window = Some(window);
        if self.redraw_requested_early {
            self.window.as_ref().unwrap().request_redraw();
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, (): ()) {
        // The guest rang the frame doorbell.
        match &self.window {
            Some(window) => window.request_redraw(),
            None => self.redraw_requested_early = true,
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => self.size = size,
            WindowEvent::CursorMoved { position, .. } => {
                self.push(EV_ABS, ABS_X, position.x.max(0.0) as u32);
                self.push(EV_ABS, ABS_Y, position.y.max(0.0) as u32);
            }
            WindowEvent::MouseInput { state, button: MouseButton::Left, .. } => {
                self.push(EV_KEY, BTN_LEFT, (state == ElementState::Pressed) as u32);
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let steps = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y as i32,
                    MouseScrollDelta::PixelDelta(p) => (p.y / 40.0) as i32,
                };
                if steps != 0 {
                    self.push(EV_REL, REL_WHEEL, steps as u32);
                }
            }
            WindowEvent::KeyboardInput { event, .. } if !event.repeat => {
                if let Some(scancode) = event.physical_key.to_scancode() {
                    let pressed = (event.state == ElementState::Pressed) as u32;
                    self.push(EV_KEY, scancode as u16, pressed);
                }
            }
            WindowEvent::RedrawRequested => {
                self.blit();
                // Dump while the guest is still parked: after release_guest()
                // it is already clearing and drawing the next frame, and the
                // dump would capture a torn mid-render framebuffer.
                let guest_parked = *self.gate.parked.lock().unwrap();
                if guest_parked && self.dump_after.is_some_and(|n| self.frame + 1 >= n) {
                    self.dump_ppm("/tmp/rust-kvm-os-frame.ppm");
                    std::process::exit(0);
                }
                self.release_guest();
            }
            _ => {}
        }
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let kernel_path = args.next().expect("usage: vmm <kernel-elf> [--dump-frames N]");
    let dump_after = match args.next().as_deref() {
        Some("--dump-frames") => {
            Some(args.next().expect("--dump-frames needs a count").parse().unwrap())
        }
        _ => None,
    };

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
    setup_vcpu(&mut vcpu, entry);

    let event_loop = EventLoop::with_user_event().build().expect("creating event loop");
    let gate = Arc::new(Gate { parked: Mutex::new(false), released: Condvar::new() });

    let vcpu_gate = gate.clone();
    let proxy = event_loop.create_proxy();
    std::thread::spawn(move || run_vcpu(vcpu, vcpu_gate, proxy));

    let mut app = App {
        mem,
        gate,
        window: None,
        surface: None,
        size: PhysicalSize::new(FB_WIDTH as u32, FB_HEIGHT as u32),
        pending: Vec::new(),
        start: Instant::now(),
        frame: 0,
        redraw_requested_early: false,
        dump_after,
    };
    event_loop.run_app(&mut app).expect("event loop");
}
