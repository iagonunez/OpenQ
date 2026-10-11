# FFmpeg in this OpenQ release

This release includes the FFmpeg libraries (libavcodec, libavformat, libavutil, libavdevice,
libavfilter, libswscale, libswresample) as separate shared libraries (DLLs on Windows, dylibs in
`OpenQ.app/Contents/Frameworks` on macOS). OpenQ uses them to play video.

- **Licence:** GNU Lesser General Public License 2.1 or later (`COPYING.LGPLv2.1` / `LICENSE.txt`
  next to this file). The FFmpeg libraries are copyright the FFmpeg developers, <https://ffmpeg.org>.
- **LGPL-only build:** FFmpeg was configured without GPL, version-3 or nonfree components (no
  x264, x265, fdk-aac, ...). The configuration is in `configure-output.txt` (macOS) or
  `BUILD.txt` (Windows).
- **Source code:** the exact FFmpeg source of these libraries is attached to the same GitHub
  release as this download (`ffmpeg-*-source.tar.*`). It is also available from
  <https://ffmpeg.org/download.html> and <https://github.com/FFmpeg/FFmpeg>.
- **Replacing the libraries:** OpenQ loads FFmpeg dynamically, so you may swap in your own build
  of the same major version. Put your DLLs next to `OpenQ.exe`, or your dylibs in
  `OpenQ.app/Contents/Frameworks` (then re-sign: `codesign --force --deep -s - OpenQ.app`).
  OpenQ's licence allows changing it and reverse engineering it to debug such changes.
- **Windows builds of FFmpeg** come from <https://github.com/BtbN/FFmpeg-Builds> ("lgpl-shared"
  variant). **macOS builds** are compiled by OpenQ's release workflow with
  `tools/build_ffmpeg_macos.sh`.
