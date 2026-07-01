#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"

# Usage: ./run.sh [paint|torus|linux] [vmm args...]
#   ./run.sh linux programs/hello
BIN=torus
if [[ $# -gt 0 && $1 != -* ]]; then
    BIN=$1
    shift
fi

(cd kernel && cargo build --release)
(cd vmm && cargo build --release)

exec vmm/target/release/vmm "kernel/target/x86_64-kernel/release/$BIN" "$@"
