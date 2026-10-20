#!/usr/bin/env bash
# Linux release build, run inside a debian:bookworm container by .github/workflows/release.yml:
#
#   docker run --rm -v "$PWD:/src" -w /src -v "<ffmpeg>:/ffmpeg:ro" debian:bookworm \
#     bash tools/release_linux.sh <target> [cargo feature flags]
#
# Debian 12 because cpal's PipeWire bindings (libspa 0.10) need PipeWire >= 0.3.65 headers (Ubuntu 22.04 has
# 0.3.48), and its glibc 2.36 is the oldest that has them: the binary runs on Debian 12+, Ubuntu 23.04+, Fedora 37+.
# FFmpeg (BtbN's LGPL shared build) is mounted at /ffmpeg; the binary finds it in lib/ next to itself (DT_RPATH).
set -euo pipefail
target=$1
shift
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq --no-install-recommends ca-certificates curl build-essential pkg-config clang libclang-dev \
  libasound2-dev libudev-dev libpipewire-0.3-dev >/dev/null
curl -sSf https://sh.rustup.rs | sh -s -- -y -q --profile minimal --default-toolchain stable
# shellcheck disable=SC1091
. "$HOME/.cargo/env"
export FFMPEG_DIR=/ffmpeg PKG_CONFIG_PATH=/ffmpeg/lib/pkgconfig
# DT_RPATH (not RUNPATH) so FFmpeg's own libraries find each other in lib/ too.
export RUSTFLAGS='-C link-arg=-Wl,-rpath,$ORIGIN/lib -C link-arg=-Wl,--disable-new-dtags'
cargo build --release --locked --target "$target" --bin openq "$@"
