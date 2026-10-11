# Package a Windows release: OpenQ.exe, the LGPL FFmpeg DLLs and the licence files, zipped.
#
# Usage: pwsh tools/package_windows.ps1 -Binary target/release/openq.exe -FFmpeg <BtbN dir> `
#                                       -Version v0.1.0 -OutDir dist
param(
  [Parameter(Mandatory)] [string] $Binary,
  [Parameter(Mandatory)] [string] $FFmpeg,
  [Parameter(Mandatory)] [string] $Version,
  [Parameter(Mandatory)] [string] $OutDir
)
$ErrorActionPreference = 'Stop'
$name = "OpenQ-$Version-windows-x64"
$stage = Join-Path ([IO.Path]::GetTempPath()) $name
if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
New-Item -ItemType Directory -Force -Path "$stage/Licenses/fonts", "$stage/Licenses/ffmpeg" | Out-Null

Copy-Item $Binary "$stage/OpenQ.exe"
# FFmpeg libraries only (no ffmpeg.exe / ffprobe.exe tools).
Get-ChildItem "$FFmpeg/bin/*.dll" | Copy-Item -Destination $stage

Copy-Item LICENSE.md, NOTICE.md, THIRD_PARTY_LICENSES.md $stage
Copy-Item assets/fonts/*.txt "$stage/Licenses/fonts/"
Copy-Item "$FFmpeg/LICENSE.txt" "$stage/Licenses/ffmpeg/LICENSE.txt"
Copy-Item packaging/FFMPEG.md "$stage/Licenses/ffmpeg/README.md"
Copy-Item packaging/README-windows.txt "$stage/README.txt"
# Record which FFmpeg build this is (BtbN folder name = version, git commit, variant).
"FFmpeg build: $(Split-Path -Leaf $FFmpeg)`nSource: https://github.com/BtbN/FFmpeg-Builds (lgpl-shared)" |
  Set-Content "$stage/Licenses/ffmpeg/BUILD.txt"

# Refuse anything but an LGPL build.
if ((Split-Path -Leaf $FFmpeg) -notmatch 'lgpl') { throw "FFmpeg build is not an LGPL variant" }

New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$zip = Join-Path $OutDir "$name.zip"
Compress-Archive -Path $stage -DestinationPath $zip -Force
Write-Output $zip
