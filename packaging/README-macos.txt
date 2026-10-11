OpenQ for macOS
===============

Drag OpenQ.app to Applications.

This build is not notarised by Apple. The first time, right-click (or Control-click) OpenQ.app
and choose Open, then Open again. If macOS says the app "is damaged", run this once in Terminal:

    xattr -dr com.apple.quarantine /Applications/OpenQ.app

macOS asks for local network access the first time remote devices or OSC are used. Allow it if
you use them.

User manual: https://github.com/iagonunez/OpenQ (docs/manual)
Licence: LICENSE.md (Apache 2.0). Third-party notices: NOTICE.md,
THIRD_PARTY_LICENSES.md and the Licenses folder (fonts, FFmpeg).
