# OpenQ notices

OpenQ
Copyright 2026 Iago Núñez — OpenQ (https://github.com/iagonunez/OpenQ)

This product is licensed under the Apache License 2.0 (see `LICENSE.md`). You may use, copy,
change, share and sell it for any purpose, including commercial productions such as paid live
shows. Keep this notice, the copyright line and `LICENSE.md` with every copy, whether changed or
not.

The parts listed below belong to their authors and keep their own licences. OpenQ's own licence
doesn't apply to them. When distributing OpenQ (source or binaries), include this file,
`LICENSE.md`, `THIRD_PARTY_LICENSES.md` and the `assets/fonts/*.txt` licence files.

## Fonts bundled in `assets/fonts` (compiled into the program)

| Font | Copyright | Licence | File |
|---|---|---|---|
| Inter (Regular, SemiBold) | © 2020 The Inter Project Authors, <https://github.com/rsms/inter> | SIL Open Font License 1.1 | `assets/fonts/OFL-Inter.txt` |
| JetBrains Mono (Medium) | © 2020 The JetBrains Mono Project Authors, <https://github.com/JetBrains/JetBrainsMono> | SIL Open Font License 1.1 | `assets/fonts/OFL-JetBrainsMono.txt` |
| Barlow Condensed (SemiBold) | © 2017 The Barlow Project Authors, <https://github.com/jpt/barlow> | SIL Open Font License 1.1 | `assets/fonts/OFL-BarlowCondensed.txt` |
| Material Symbols Rounded (outlined and filled) | © Google LLC, <https://github.com/google/material-design-icons> | Apache License 2.0 | `assets/fonts/LICENSE-MaterialSymbols.txt` |

**Modifications:** `tools/build_fonts.py` changes these fonts.
- It fixes the variable fonts at one weight. Material Symbols is also fixed at one fill and optical size.
- It keeps only the characters and icons OpenQ uses.

The modified fonts keep their original names, since none of these fonts declares a Reserved Font
Name. The fonts remain under their own licences, and they are not sold on their own.

## Fonts built into egui (compiled into the program)

These come with the `epaint_default_fonts` crate. Their licence texts are reproduced in
`THIRD_PARTY_LICENSES.md`.

- **Hack:** © 2018 Source Foundry Authors, MIT License. It includes Bitstream Vera Sans Mono
  (© 2003 Bitstream, Inc., Bitstream Vera License) and DejaVu changes (public domain).
- **Ubuntu Light:** © 2011 Canonical Ltd., Ubuntu Font Licence 1.0.
- **Noto Emoji:** © 2013 Google Inc., SIL Open Font License 1.1.
- **emoji-icon-font:** © 2014 John Slegers, MIT License.

## Rust crates (compiled into the program)

OpenQ is built from 541 open-source crates. Most are under MIT, Apache-2.0, BSD, Zlib, ISC or
Unicode-3.0 terms. `THIRD_PARTY_LICENSES.md` lists every crate with its version, licence and source
link, followed by the full licence texts. Regenerate it with `python3 -I tools/third_party_licenses.py`.

Notable cases:
- **Symphonia** (audio decoding, 17 crates) and **serialport** are under the Mozilla Public
  License 2.0. OpenQ uses them unmodified. Their source is available from crates.io and from
  <https://github.com/pdeljanov/Symphonia> and <https://github.com/serialport/serialport-rs>.
- Some crates offer several licences (for example "Apache-2.0 OR GPL-2.0" for `self_cell`, and
  "MIT OR GPL-3.0" for `unescaper`). OpenQ uses them under their permissive option.

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

  The official Windows and macOS releases do this. They bundle FFmpeg 9.0 shared libraries
  configured without GPL, version-3 or nonfree parts. The release workflow refuses any other
  configuration. Each zip has the LGPL text and build details under `Licenses/ffmpeg`, and the
  release page carries the exact FFmpeg source. The libraries stay replaceable, as the LGPL
  requires (see `packaging/FFMPEG.md`).
- **Audio, windowing and graphics:** the operating system's own libraries (ALSA/PipeWire, X11,
  Wayland, Vulkan/OpenGL, CoreAudio, WASAPI…), under their own terms.
- **System fonts:** Text cues can use any font installed on the computer. The show file stores
  only the font's name. Each font's licence governs its use on stage.

## Media formats and patents

OpenQ decodes audio with Symphonia: WAV, FLAC, MP3, Vorbis, ALAC, AAC and others. Patents on MP3
have expired. Some countries may still have patents on AAC. If you distribute binaries
commercially where those apply, think about patent licensing, or build Symphonia without the
`aac` feature. Video codecs come from the system FFmpeg (see above) and aren't part of OpenQ.

## User manual

`docs/manual` is part of OpenQ and is under the same licence. Its screenshots show OpenQ itself. The web
page loads Inter, JetBrains Mono, Barlow Condensed and Material Symbols from Google Fonts, under
the licences above.
