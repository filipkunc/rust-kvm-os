#!/usr/bin/env bash
# Build the static musl test binary in an Alpine container.
set -euo pipefail
cd "$(dirname "$0")"
podman run --rm -v "$PWD:/w:Z" docker.io/library/alpine:latest \
    sh -c 'apk add -q gcc musl-dev && gcc -static -no-pie -O2 -o /w/hello /w/hello.c'
