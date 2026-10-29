#!/bin/bash
# Installs what firecracker's musl build needs on a stock Ubuntu 24.04 host.
# Mirrors the relevant parts of tools/devctr/Dockerfile, so CI does not depend
# on pulling the fcuvm image from ECR Public, whose anonymous per-IP data limit
# GitHub's shared runners regularly exhaust.
set -euxo pipefail

REPO=$(cd "$(dirname "$0")/../.." && pwd)
ARCH=$(uname -m)
LIBSECCOMP_VER=v2.6.1

SUDO=""
[ "$(id -u)" -eq 0 ] || SUDO=sudo

$SUDO apt-get update
$SUDO apt-get -y install --no-install-recommends \
    git gcc make libc-dev ca-certificates curl file musl-tools clang cmake \
    libtool gperf autoconf automake

# Help musl-gcc find the Linux headers.
cd /usr/include/$ARCH-linux-musl
$SUDO ln -sfn ../$ARCH-linux-gnu/asm asm
$SUDO ln -sfn ../linux linux
$SUDO ln -sfn ../asm-generic asm-generic

# Static libseccomp built with musl-gcc; seccompiler links it from /usr/local/lib.
git clone --depth 1 --branch $LIBSECCOMP_VER https://github.com/seccomp/libseccomp /tmp/libseccomp
cd /tmp/libseccomp
./autogen.sh
CC="musl-gcc -static" ./configure --enable-static=yes --enable-shared=false
make -j"$(nproc)"
$SUDO make install

# Rust toolchain and musl targets as pinned in rust-toolchain.toml.
if ! command -v rustup >/dev/null; then
    curl https://sh.rustup.rs -sSf | sh -s -- -y --profile minimal --default-toolchain none
    echo "$HOME/.cargo/bin" >> "${GITHUB_PATH:-/dev/null}"
    export PATH="$HOME/.cargo/bin:$PATH"
fi
cd "$REPO"
rustup toolchain install
rustup show
