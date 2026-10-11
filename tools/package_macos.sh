#!/usr/bin/env bash
# Package a macOS release: OpenQ.app with the LGPL FFmpeg libraries in Contents/Frameworks,
# zipped with the licence files.
#
# Usage: tools/package_macos.sh <binary> <ffmpeg prefix> <version> <arch> <out dir>
set -euo pipefail

bin="$1"; ff="$2"; version="$3"; arch="$4"; out="$5"
name="OpenQ-${version}-macos-${arch}"
stage="$(mktemp -d)/$name"
app="$stage/OpenQ.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Frameworks" "$app/Contents/Resources"

cp "$bin" "$app/Contents/MacOS/OpenQ"
# The FFmpeg dylibs (real files and their version symlinks).
cp -a "$ff"/lib/*.dylib "$app/Contents/Frameworks/"
install_name_tool -add_rpath "@executable_path/../Frameworks" "$app/Contents/MacOS/OpenQ"

cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>OpenQ</string>
  <key>CFBundleDisplayName</key><string>OpenQ</string>
  <key>CFBundleIdentifier</key><string>io.github.iagonunez.openq</string>
  <key>CFBundleExecutable</key><string>OpenQ</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>${version#v}</string>
  <key>CFBundleVersion</key><string>${version#v}</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSHumanReadableCopyright</key><string>Copyright 2026 iagonunez. Apache-2.0.</string>
  <key>NSLocalNetworkUsageDescription</key><string>OpenQ uses the local network for remote devices and OSC show control.</string>
</dict>
</plist>
EOF

# Licences: OpenQ's, the third-party list, the bundled fonts' and FFmpeg's (with its source).
lic="$stage/Licenses"
mkdir -p "$lic/fonts" "$lic/ffmpeg"
cp LICENSE.md NOTICE.md THIRD_PARTY_LICENSES.md "$stage/"
cp assets/fonts/*.txt "$lic/fonts/"
cp "$ff/src/COPYING.LGPLv2.1" "$ff/src/LICENSE.md" "$ff/src/configure-output.txt" "$lic/ffmpeg/"
cp packaging/FFMPEG.md "$lic/ffmpeg/README.md"
cp packaging/README-macos.txt "$stage/README.txt"
# The manual is also built into the app (Help → User manual); a copy to read without it.
mkdir -p "$stage/Manual/fonts"
cp -R docs/manual/. "$stage/Manual/"
cp assets/fonts/MaterialSymbolsRounded.ttf "$stage/Manual/fonts/"
cp "$lic"/ffmpeg/* "$app/Contents/Resources/" 2>/dev/null || true

# install_name_tool invalidates signatures: sign the whole bundle ad hoc (required on Apple
# silicon). It is not notarised, so the first launch needs right-click → Open.
codesign --force --deep --sign - "$app"
codesign --verify --deep --strict "$app"

# Check the app only loads FFmpeg from inside the bundle.
otool -L "$app/Contents/MacOS/OpenQ" | grep -E 'libav|libsw' | grep -v '@rpath' && { echo "absolute FFmpeg path in binary" >&2; exit 1; } || true

mkdir -p "$out"
(cd "$(dirname "$stage")" && ditto -c -k --keepParent "$name" "$out/$name.zip")
echo "$out/$name.zip"
