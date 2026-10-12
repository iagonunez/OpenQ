#!/usr/bin/env bash
# Build an LGPL-only, shared FFmpeg for macOS release builds.
#
# Usage: tools/build_ffmpeg_macos.sh <version> <prefix>
#   e.g. tools/build_ffmpeg_macos.sh 9.0.2 "$PWD/ffmpeg-out"
#
# The configuration leaves out GPL and nonfree code (FFmpeg's defaults, made explicit) and turns
# off auto-detection, so no extra library on the build machine (x264, x265, fdk-aac, ...) can sneak
# in. The libraries get `@rpath` install names, so the app bundle can load them from
# Contents/Frameworks. The exact source tarball is kept at <prefix>/src for the release page
# (LGPL: the source of the bundled libraries must be offered).
set -euo pipefail

version="$1"
prefix="$2"
work="$(mktemp -d)"
tarball="ffmpeg-${version}.tar.xz"

mkdir -p "$prefix/src"
curl -fsSL "https://ffmpeg.org/releases/${tarball}" -o "$prefix/src/${tarball}"
curl -fsSL "https://ffmpeg.org/releases/${tarball}.asc" -o "$prefix/src/${tarball}.asc" || true
tar -xf "$prefix/src/${tarball}" -C "$work"
cd "$work/ffmpeg-${version}"

./configure \
  --prefix="$prefix" \
  --install-name-dir='@rpath' \
  --enable-shared --disable-static \
  --disable-gpl --disable-nonfree --disable-version3 \
  --disable-programs --disable-doc \
  --disable-autodetect \
  --enable-videotoolbox --enable-audiotoolbox \
  --enable-avfoundation \
  --enable-zlib --enable-bzlib \
  --enable-pic \
  | tee "$prefix/src/configure-output.txt"

# Refuse to continue if the result is anything but LGPL.
grep -q '^License: LGPL version 2.1 or later' "$prefix/src/configure-output.txt" \
  || { echo "FFmpeg configuration is not LGPL-only" >&2; exit 1; }

make -j"$(sysctl -n hw.ncpu)"
make install
cp COPYING.LGPLv2.1 LICENSE.md "$prefix/src/"
echo "FFmpeg ${version} (LGPL 2.1+) installed to $prefix"
