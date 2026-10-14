#!/usr/bin/env python3
"""Generate the GitHub Pages landing pages (docs/index.html, docs/es/, docs/gl/) from one
template, so the three languages keep the same layout. Run `python3 -I tools/build_site.py`
after changing the texts below; docs/site/ holds the shared CSS, JS and logo."""

from pathlib import Path

DOCS = Path(__file__).resolve().parent.parent / "docs"
REPO = "https://github.com/iagonunez/OpenQ"
SH = "curl --proto '=https' --tlsv1.2 -fsSL https://github.com/iagonunez/oqman/releases/latest/download/install.sh | sh"
PS = 'powershell -ExecutionPolicy Bypass -c "irm https://github.com/iagonunez/oqman/releases/latest/download/install.ps1 | iex"'

TEXT = {
    "en": {
        "dir": "", "readme": "README.md", "manual": "manual/index.html",
        "title": "OpenQ — live show control for sound, video and the stage",
        "desc": "OpenQ runs a show from a list of cues: music, sound effects, video, text, fades and OSC, fired with one GO button. Free for any production.",
        "tagline": "Live show control for sound, video and the stage",
        "lede": "Build a list of cues — music, effects, movies, titles, fades, messages to the lighting desk — and run the whole show with one GO button. Free for any production, commercial ones included.",
        "get": "Install", "source": "Source code", "manual_l": "User manual", "license": "License",
        "install": "Install OpenQ",
        "where_unix": "In a terminal:", "where_win": "In PowerShell:",
        "after_linux": "Installs the build tools from your distribution's packages (it asks first), builds OpenQ and adds it to your applications menu.",
        "after_macos": "macOS 11 or later. On a Mac without Apple's Command Line Tools, macOS asks to install them first; run the command again afterwards. OpenQ ends up in Applications in your home folder.",
        "after_windows": "Windows 10/11, 64-bit. Without Microsoft's C++ Build Tools, their installer opens first: accept it and let it finish. OpenQ is then in the Start menu.",
        "more": "The installer, <a href=\"https://github.com/iagonunez/oqman\">oqman</a>, builds OpenQ on your computer. Later: <code>oqman update</code> · <code>oqman uninstall</code>.",
        "copy": "Copy", "copied": "Copied",
        "shot_alt": "OpenQ running a show: the GO button, the standby cue, a cue list with audio, video, text, group, fade and network cues, and the cues that are playing",
        "what": "What it does",
        "features": [
            ("15 cue types", "Audio, video, text, camera, live mic, groups, waits, fades, OSC and more."),
            ("Sample-accurate timing", "Pre- and post-waits, auto-continue and auto-follow, chained to the exact sample."),
            ("Mixer, EQ and effects", "Routing matrix, 8-band EQ, reverb, delay, dynamics and more, edited live."),
            ("Object audio", "Place sounds in a room of speakers and fly them around with fades."),
            ("Video and projection", "Movies, stills, SVG and titles on any number of stages, with mapping and edge blending."),
            ("Animation and video effects", "Keyframes, blend modes, colour effects and chroma key, drawn live by the GPU."),
            ("Timecode and OSC", "Follow LTC or MTC, take OSC from other gear and send it to lighting desks."),
            ("Remote devices", "Run or edit the show from a laptop or tablet over an encrypted link."),
            ("Built for the booth", "Show mode, autosave and crash recovery, audio that reconnects by itself."),
        ],
        "more_features": "Everything is explained step by step in the <a href=\"{manual}\">user manual</a>, which also comes with the app (Help → User manual, or F1).",
        "foot": "Apache 2.0: free to use, change, share and sell, including for commercial productions.",
    },
    "es": {
        "dir": "es/", "readme": "README.es.md", "manual": "manual/index.es.html",
        "title": "OpenQ — control de espectáculos en directo para sonido, vídeo y escena",
        "desc": "OpenQ lleva un espectáculo desde una lista de cues: música, efectos, vídeo, texto, fundidos y OSC, con un solo botón GO. Gratis para cualquier producción.",
        "tagline": "Control de espectáculos en directo para sonido, vídeo y escena",
        "lede": "Monta una lista de cues — música, efectos, películas, rótulos, fundidos, mensajes a la mesa de luces — y lleva todo el espectáculo con un solo botón GO. Gratis para cualquier producción, también comercial.",
        "get": "Instalar", "source": "Código fuente", "manual_l": "Manual de usuario", "license": "Licencia",
        "install": "Instala OpenQ",
        "where_unix": "En un terminal:", "where_win": "En PowerShell:",
        "after_linux": "Instala las herramientas de compilación con los paquetes de tu distribución (pregunta antes), compila OpenQ y lo añade al menú de aplicaciones.",
        "after_macos": "macOS 11 o posterior. En un Mac sin las Command Line Tools de Apple, macOS pide instalarlas primero; después vuelve a ejecutar la orden. OpenQ queda en Aplicaciones dentro de tu carpeta personal.",
        "after_windows": "Windows 10/11 de 64 bits. Sin las C++ Build Tools de Microsoft, primero se abre su instalador: acéptalo y deja que termine. Después OpenQ está en el menú Inicio.",
        "more": "El instalador, <a href=\"https://github.com/iagonunez/oqman\">oqman</a>, compila OpenQ en tu ordenador. Más adelante: <code>oqman update</code> · <code>oqman uninstall</code>.",
        "copy": "Copiar", "copied": "Copiado",
        "shot_alt": "OpenQ llevando un espectáculo: el botón GO, la cue en espera, una lista con cues de audio, vídeo, texto, grupo, fundido y red, y las cues que están sonando",
        "what": "Qué hace",
        "features": [
            ("15 tipos de cue", "Audio, vídeo, texto, cámara, micro en directo, grupos, esperas, fundidos, OSC y más."),
            ("Tiempos a la muestra", "Pre y post-esperas, autocontinuar y autoseguir, encadenados a la muestra exacta."),
            ("Mezclador, EQ y efectos", "Matriz de enrutado, EQ de 8 bandas, reverb, delay, dinámica y más, editados en directo."),
            ("Audio por objetos", "Coloca sonidos en una sala de altavoces y muévelos con fundidos."),
            ("Vídeo y proyección", "Películas, imágenes, SVG y rótulos en tantos escenarios como quieras, con mapping y edge blending."),
            ("Animación y efectos de vídeo", "Fotogramas clave, modos de fusión, efectos de color y croma, dibujados en directo por la GPU."),
            ("Código de tiempo y OSC", "Sigue LTC o MTC, recibe OSC de otros equipos y envíalo a mesas de luces."),
            ("Dispositivos remotos", "Lleva o edita el espectáculo desde un portátil o una tableta con conexión cifrada."),
            ("Hecho para la cabina", "Modo función, autoguardado y recuperación tras fallos, audio que se reconecta solo."),
        ],
        "more_features": "Todo se explica paso a paso en el <a href=\"{manual}\">manual de usuario</a>, que también viene con la aplicación (Ayuda → Manual de usuario, o F1).",
        "foot": "Apache 2.0: uso gratuito, también en producciones comerciales; puedes modificarlo, compartirlo y venderlo.",
    },
    "gl": {
        "dir": "gl/", "readme": "README.gl.md", "manual": "manual/index.gl.html",
        "title": "OpenQ — control de espectáculos en directo para son, vídeo e escena",
        "desc": "OpenQ leva un espectáculo desde unha lista de cues: música, efectos, vídeo, texto, fundidos e OSC, cun só botón GO. De balde para calquera produción.",
        "tagline": "Control de espectáculos en directo para son, vídeo e escena",
        "lede": "Monta unha lista de cues — música, efectos, películas, rótulos, fundidos, mensaxes á mesa de luces — e leva todo o espectáculo cun só botón GO. De balde para calquera produción, tamén comercial.",
        "get": "Instalar", "source": "Código fonte", "manual_l": "Manual de usuario", "license": "Licenza",
        "install": "Instala OpenQ",
        "where_unix": "Nun terminal:", "where_win": "En PowerShell:",
        "after_linux": "Instala as ferramentas de compilación cos paquetes da túa distribución (pregunta antes), compila OpenQ e engádeo ao menú de aplicacións.",
        "after_macos": "macOS 11 ou posterior. Nun Mac sen as Command Line Tools de Apple, macOS pide instalalas primeiro; despois volve executar a orde. OpenQ queda en Aplicacións dentro do teu cartafol persoal.",
        "after_windows": "Windows 10/11 de 64 bits. Sen as C++ Build Tools de Microsoft, primeiro ábrese o seu instalador: acéptao e deixa que remate. Despois OpenQ está no menú Inicio.",
        "more": "O instalador, <a href=\"https://github.com/iagonunez/oqman\">oqman</a>, compila OpenQ no teu ordenador. Máis adiante: <code>oqman update</code> · <code>oqman uninstall</code>.",
        "copy": "Copiar", "copied": "Copiado",
        "shot_alt": "OpenQ levando un espectáculo: o botón GO, a cue en espera, unha lista con cues de audio, vídeo, texto, grupo, fundido e rede, e as cues que están a soar",
        "what": "Que fai",
        "features": [
            ("15 tipos de cue", "Audio, vídeo, texto, cámara, micro en directo, grupos, esperas, fundidos, OSC e máis."),
            ("Tempos á mostra", "Pre e post-esperas, autocontinuar e autoseguir, encadeados á mostra exacta."),
            ("Mesturador, EQ e efectos", "Matriz de enrutado, EQ de 8 bandas, reverb, delay, dinámica e máis, editados en directo."),
            ("Audio por obxectos", "Coloca sons nunha sala de altofalantes e móveos con fundidos."),
            ("Vídeo e proxección", "Películas, imaxes, SVG e rótulos en tantos escenarios como queiras, con mapping e edge blending."),
            ("Animación e efectos de vídeo", "Fotogramas clave, modos de fusión, efectos de cor e croma, debuxados en directo pola GPU."),
            ("Código de tempo e OSC", "Segue LTC ou MTC, recibe OSC doutros equipos e envíao a mesas de luces."),
            ("Dispositivos remotos", "Leva ou edita o espectáculo desde un portátil ou unha tableta cunha conexión cifrada."),
            ("Feito para a cabina", "Modo función, autogardado e recuperación tras fallos, audio que se reconecta só."),
        ],
        "more_features": "Todo se explica paso a paso no <a href=\"{manual}\">manual de usuario</a>, que tamén vén coa aplicación (Axuda → Manual de usuario, ou F1).",
        "foot": "Apache 2.0: uso de balde, tamén en producións comerciais; podes modificalo, compartilo e vendelo.",
    },
}

LANG_NAMES = [("en", "EN", "English"), ("es", "ES", "Español"), ("gl", "GL", "Galego")]


def esc(s):
    return s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;").replace('"', "&quot;")


def page(lang):
    t = TEXT[lang]
    up = "../" if t["dir"] else ""
    manual = up + t["manual"]
    langs = "".join(
        f'<a href="{up}{TEXT[c]["dir"]}" hreflang="{c}" title="{n}"{" aria-current=\"page\"" if c == lang else ""}>{short}</a>'
        for c, short, n in LANG_NAMES
    )
    alternates = "\n".join(
        f'<link rel="alternate" hreflang="{c}" href="https://iagonunez.github.io/OpenQ/{TEXT[c]["dir"]}">' for c, _, _ in LANG_NAMES
    )

    def panel(os, emoji, name, where, cmd, after, ps=False):
        return (
            f'<div class="panel" id="panel-{os}" role="tabpanel" aria-labelledby="tab-{os}"{"" if os == "linux" else " hidden"}>\n'
            f'  <p class="where">{where}</p>\n'
            f'  <div class="cmd{" ps" if ps else ""}"><code>{esc(cmd)}</code>'
            f'<button class="copy" type="button" data-done="{t["copied"]}">{t["copy"]}</button></div>\n'
            f'  <p class="after">{after}</p>\n</div>'
        )

    tabs = "".join(
        f'<button role="tab" id="tab-{os}" aria-controls="panel-{os}" aria-selected="{"true" if os == "linux" else "false"}"'
        f'{"" if os == "linux" else " tabindex=\"-1\""}><span aria-hidden="true">{emoji}</span>{name}</button>'
        for os, emoji, name in [("linux", "🐧", "Linux"), ("macos", "🍎", "macOS"), ("windows", "🪟", "Windows")]
    )
    panels = "\n".join(
        [
            panel("linux", "🐧", "Linux", t["where_unix"], SH, t["after_linux"]),
            panel("macos", "🍎", "macOS", t["where_unix"], SH, t["after_macos"]),
            panel("windows", "🪟", "Windows", t["where_win"], PS, t["after_windows"], ps=True),
        ]
    )
    features = "\n".join(f'  <div class="feature"><b>{esc(a)}</b><span>{esc(b)}</span></div>' for a, b in t["features"])
    return f"""<!doctype html>
<html lang="{lang}">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{esc(t["title"])}</title>
<meta name="description" content="{esc(t["desc"])}">
<meta property="og:title" content="OpenQ">
<meta property="og:description" content="{esc(t["desc"])}">
<meta property="og:image" content="https://iagonunez.github.io/OpenQ/img/screenshot.webp">
<meta name="theme-color" content="#141518">
{alternates}
<link rel="icon" href="{up}site/icon.svg" type="image/svg+xml">
<link rel="alternate icon" href="{up}favicon.ico" sizes="16x16 24x24 32x32 48x48">
<link rel="preconnect" href="https://fonts.googleapis.com">
<link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
<link rel="stylesheet" href="https://fonts.googleapis.com/css2?family=Barlow+Condensed:wght@600&family=Inter:wght@400;600&family=JetBrains+Mono:wght@400&display=swap">
<link rel="stylesheet" href="{up}site/site.css">
</head>
<body>
<header class="top">
  <a href="{up}{t["dir"]}" aria-label="OpenQ"><img class="mini" src="{up}site/logo.svg" alt="OpenQ"></a>
  <nav>
    <a class="hide-sm" href="#install">{t["get"]}</a>
    <a class="hide-sm" href="{manual}">{t["manual_l"]}</a>
    <a href="{REPO}">GitHub</a>
    <span class="langs">{langs}</span>
  </nav>
</header>

<main>
<section class="hero">
  <img class="logo" src="{up}site/logo.svg" alt="OpenQ" width="340" height="140">
  <h1>{esc(t["tagline"])}</h1>
  <p>{esc(t["lede"])}</p>
  <div class="badges">
    <a class="btn go" href="#install">{t["get"]}</a>
    <a class="btn" href="{manual}">📖 {t["manual_l"]}</a>
    <a class="btn" href="{REPO}">⭐ {t["source"]}</a>
  </div>
</section>

<section class="install" id="install">
  <h2>{t["install"]}</h2>
  <div class="card">
    <div class="tabs" role="tablist" aria-label="{t["install"]}">{tabs}</div>
{panels}
  </div>
  <p class="more">{t["more"]}</p>
</section>

<div class="shot"><img src="{up}img/screenshot.webp" width="1600" height="1466" alt="{esc(t["shot_alt"])}" loading="lazy"></div>

<section class="section">
  <h2>{t["what"]}</h2>
  <div class="features">
{features}
  </div>
  <p class="more">{t["more_features"].format(manual=manual)}</p>
</section>
</main>

<footer>
  <span>© 2026 Iago Núñez · <a href="{REPO}/blob/master/LICENSE.md">{t["license"]}</a>: {esc(t["foot"])}</span>
  <span><a href="{REPO}/blob/master/{t["readme"]}">README</a> · <a href="{REPO}">GitHub</a></span>
</footer>
<script src="{up}site/site.js"></script>
</body>
</html>
"""


for lang, t in TEXT.items():
    out = DOCS / t["dir"] / "index.html"
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(page(lang), encoding="utf-8")
    print(out.relative_to(DOCS.parent))
