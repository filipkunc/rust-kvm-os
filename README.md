# rust-kvm-os

Both sides of a machine on Linux KVM: a tiny VMM talking to `/dev/kvm` directly (no QEMU), and a `no_std` Rust kernel it boots straight into 64-bit long mode. No BIOS, no bootloader, no firmware — the VMM presets page tables, segment state, stack pointer, and SSE enable bits, so the first instruction that ever executes is the kernel's `_start`.

```
./run.sh
```

```
hello from ring 0
sqrt(3*3 + 4*4) = 5
[vmm] guest requested exit with code 0
```

## Layout

- `vmm/` — the hypervisor. Stable Rust, [kvm-ioctls](https://github.com/rust-vmm/kvm). Loads the kernel ELF into guest memory, builds a 1 GiB identity map (3 pages), configures the vCPU for long mode, and services port-IO exits (serial output, exit request).
- `kernel/` — the guest. `no_std`, no assembly. Built with a custom hardfloat target spec (`x86_64-kernel.json`) on nightly via `-Zbuild-std`, because the stock `x86_64-unknown-none` target is soft-float.

## Requirements

- Linux with `/dev/kvm` accessible (world-rw on Fedora by default; `kvm` group on Debian/Ubuntu)
- Rust stable (VMM) and nightly with `rust-src` (kernel; `rust-toolchain.toml` handles it)
