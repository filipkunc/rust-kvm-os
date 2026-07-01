#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"

(cd kernel && cargo build --release)
(cd vmm && cargo build --release)

exec vmm/target/release/vmm kernel/target/x86_64-kernel/release/kernel "$@"
