# OpenQ notices

OpenQ
Copyright 2026 Iago Núñez — OpenQ (https://github.com/iagonunez/OpenQ)

This product is licensed under the Apache License 2.0 (see `LICENSE.md`). You may use, copy,
change, share and sell it for any purpose, including commercial productions such as paid live
shows. Keep this notice, the copyright line and `LICENSE.md` with every copy, whether changed or
not.

The parts listed below belong to their authors and keep their own licenses. OpenQ's own license
doesn't apply to them. When distributing OpenQ (source or binaries), include this file,
`LICENSE.md`, `THIRD_PARTY_LICENSES.md` and the `assets/fonts/*.txt` license files.

## Fonts bundled in `assets/fonts` (compiled into the program)

| Font | Copyright | License | File |
|---|---|---|---|
| Inter (Regular, SemiBold) | © 2020 The Inter Project Authors, <https://github.com/rsms/inter> | SIL Open Font License 1.1 | `assets/fonts/OFL-Inter.txt` |
| JetBrains Mono (Medium) | © 2020 The JetBrains Mono Project Authors, <https://github.com/JetBrains/JetBrainsMono> | SIL Open Font License 1.1 | `assets/fonts/OFL-JetBrainsMono.txt` |
| Barlow Condensed (SemiBold) | © 2017 The Barlow Project Authors, <https://github.com/jpt/barlow> | SIL Open Font License 1.1 | `assets/fonts/OFL-BarlowCondensed.txt` |
| Material Symbols Rounded (outlined and filled) | © Google LLC, <https://github.com/google/material-design-icons> | Apache License 2.0 | `assets/fonts/LICENSE-MaterialSymbols.txt` |

**Modifications:** `tools/build_fonts.py` changes these fonts.
- It fixes the variable fonts at one weight. Material Symbols is also fixed at one fill and optical size.
- It keeps only the characters and icons OpenQ uses.

The modified fonts keep their original names, since none of these fonts declares a Reserved Font
Name. The fonts remain under their own licenses, and they are not sold on their own.

## Fonts built into egui (compiled into the program)

These come with the `epaint_default_fonts` crate. Their license texts are reproduced in
`THIRD_PARTY_LICENSES.md`.

- **Hack:** © 2018 Source Foundry Authors, MIT License. It includes Bitstream Vera Sans Mono
  (© 2003 Bitstream, Inc., Bitstream Vera License) and DejaVu changes (public domain).
- **Ubuntu Light:** © 2011 Canonical Ltd., Ubuntu Font Licence 1.0.
- **Noto Emoji:** © 2013 Google Inc., SIL Open Font License 1.1.
- **emoji-icon-font:** © 2014 John Slegers, MIT License.

## Rust crates (compiled into the program)

OpenQ is built from 647 open-source crates. Most are under MIT, Apache-2.0, BSD, Zlib, ISC or
Unicode-3.0 terms. `THIRD_PARTY_LICENSES.md` lists every crate with its version, license and source
link, followed by the full license texts. Regenerate it with `python3 -I tools/third_party_licenses.py`.

Notable cases:
- **Symphonia** (audio decoding, 17 crates) and **serialport** are under the Mozilla Public
  License 2.0. OpenQ uses them unmodified. Their source is available from crates.io and from
  <https://github.com/pdeljanov/Symphonia> and <https://github.com/serialport/serialport-rs>.
- Some crates offer several licenses (for example "Apache-2.0 OR GPL-2.0" for `self_cell`, and
  "MIT OR GPL-3.0" for `unescaper`). OpenQ uses them under their permissive option.

- **Wasmtime** (the sandbox effect scripts run in, Bytecode Alliance) is under
  Apache-2.0 WITH LLVM-exception.

## Python for effect scripts (installed next to the app, not compiled in)

oqman installs CPython compiled to WebAssembly (`python.wasm` and part of its standard library,
from <https://github.com/brettcannon/cpython-wasi-build>) in a `python` folder next to OpenQ, with
its license in `Licenses/python`. CPython is under the Python Software Foundation License. OpenQ
only runs it inside its Wasmtime sandbox, for lighting effect scripts.

## System libraries (not included, used from the computer)

- **FFmpeg** (video; libavformat, libavcodec, libavutil, libswscale, libswresample) is loaded
  from the system and is not part of this repository. FFmpeg is licensed under LGPL 2.1 or
  later. Builds made with `--enable-gpl` (common in Linux distributions, often with x264/x265)
  are under GPL 2 or 3 instead.

  > **Distributing OpenQ binaries:** link and ship them against an **LGPL-only** FFmpeg build
  > (configured without `--enable-gpl` and `--enable-nonfree`), or build with
  > `--no-default-features` (no video). Apache 2.0 is compatible with GPL 3 (not with
  > GPL 2 only), so a binary combined with a GPL FFmpeg can be distributed, but only under GPL 3
  > as a whole, with its complete source.
  > Distributing the source code is not affected.

  OpenQ isn't distributed as binaries: the installer, oqman
  (<https://github.com/iagonunez/oqman>), builds it on the user's own computer, which is not
  distribution. On Windows and macOS it still uses FFmpeg 9.0 shared libraries configured without
  GPL, version-3 or nonfree parts (BtbN's `lgpl-shared` build on Windows; compiled from the
  official source on macOS) and refuses any other configuration, so the installed app can be
  passed on: it carries the LGPL text and build details under `Licenses/ffmpeg`, the source is at
  <https://ffmpeg.org>, and the libraries stay replaceable, as the LGPL requires. On Linux it uses
  the distribution's FFmpeg (often GPL): such a build can only be passed on under GPL 3, with its
  source.
- **Audio, windowing and graphics:** the operating system's own libraries (ALSA/PipeWire, X11,
  Wayland, Vulkan/OpenGL, CoreAudio, WASAPI…), under their own terms.
- **System fonts:** Text cues can use any font installed on the computer. The show file stores
  only the font's name. Each font's license governs its use on stage.

## Media formats and patents

OpenQ decodes audio with Symphonia: WAV, FLAC, MP3, Vorbis, ALAC, AAC and others. Patents on MP3
have expired. Some countries may still have patents on AAC. If you distribute binaries
commercially where those apply, think about patent licensing, or build Symphonia without the
`aac` feature. Video codecs come from the system FFmpeg (see above) and aren't part of OpenQ.

## User manual

`docs/manual` is part of OpenQ and is under the same license. Its screenshots show OpenQ itself. The web
page loads Inter, JetBrains Mono, Barlow Condensed and Material Symbols from Google Fonts, under
the licenses above.
