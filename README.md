# rust-kvm-os

Both sides of a machine on Linux KVM: a tiny VMM talking to `/dev/kvm` directly (no QEMU), and a `no_std` Rust kernel it boots straight into 64-bit long mode. No BIOS, no bootloader, no firmware — the VMM presets page tables, segment state, stack pointer, and SSE enable bits, so the first instruction that ever executes is the kernel's `_start`.

```
./run.sh            # spinning torus, software-rasterized in ring 0
./run.sh paint      # mouse paint program
./run.sh linux programs/hello   # run an unmodified static Linux binary
```

The torus and paint demos open a window: the guest renders into a framebuffer in its RAM, signals end-of-frame on a port-IO doorbell, and receives evdev-shaped mouse/keyboard events plus time through a shared-memory mailbox. The `linux` kernel instead loads a static Linux ELF, sets up a SysV process stack and the syscall MSRs, and translates ~20 syscalls (write goes to serial, exit_group becomes the exit port):

```
$ ./run.sh linux programs/hello   # hello.c built with musl in a container
hello from Linux userspace
  1 squared is 1
  ...
[kernel] program exited with 42
```

## Layout

- `vmm/` — the hypervisor. Stable Rust, [kvm-ioctls](https://github.com/rust-vmm/kvm). Loads the kernel ELF into guest memory, builds a 1 GiB identity map (3 pages), configures the vCPU for long mode, blits the guest framebuffer into a winit/softbuffer window, and services port-IO exits.
- `kernel/` — the guests: `paint`, `torus`, `linux` bins over a tiny lib. `no_std`; the only assembly in the repo is the ~20-line syscall entry stub. Built with a custom hardfloat target spec (`x86_64-kernel.json`) on nightly via `-Zbuild-std`, because the stock `x86_64-unknown-none` target is soft-float.
- `shared/` — the guest/host ABI: memory map, port numbers, event and mailbox structs.
- `programs/` — Linux test binaries; `build.sh` compiles them statically with musl in an Alpine container.

## Requirements

- Linux with `/dev/kvm` accessible (world-rw on Fedora by default; `kvm` group on Debian/Ubuntu)
- Rust stable (VMM) and nightly with `rust-src` (kernel; `rust-toolchain.toml` handles it)
