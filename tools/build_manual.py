#!/usr/bin/env python3
"""Rebuild the user manual's pages (docs/manual) around their content.

The manual is one page per chapter in each language (`<chapter>.html`, `<chapter>.es.html`,
`<chapter>.gl.html`) plus a contents page (`index*.html`). Only the part of each page between
`<!--content-->` and `<!--/content-->` is written by hand; this script rewrites everything else:
the head, the chapter rail with the current chapter's sections and subsections, the chapter
heading, the previous/next links and, on the contents page, the list of every section and
subsection. It also numbers the sections (chapter.section), gives every <h3> an id (the English
page's ids are reused by the translations, so the same link works in every language) and points
`#id` links at the page that holds the id.

Run it after editing any page: python3 -I tools/build_manual.py
"""

import html
import re
import sys
from pathlib import Path

DIR = Path(__file__).resolve().parent.parent / "docs" / "manual"

LANGS = ["en", "es", "gl"]
LANG_NAMES = {"en": ("EN", "English"), "es": ("ES", "Español"), "gl": ("GL", "Galego")}

# Chapters in reading order: file stem and title in each language.
CHAPTERS = [
    ("start", {"en": "Getting started", "es": "Primeros pasos", "gl": "Primeiros pasos"}),
    ("cues", {"en": "Cues and timing", "es": "Cues y tiempos", "gl": "Cues e tempos"}),
    ("audio", {"en": "Audio", "es": "Audio", "gl": "Audio"}),
    ("video", {"en": "Video and projection", "es": "Vídeo y proyección", "gl": "Vídeo e proxección"}),
    ("lighting", {"en": "Lighting", "es": "Iluminación", "gl": "Iluminación"}),
    ("remote", {"en": "Remote control and network", "es": "Control remoto y red", "gl": "Control remoto e rede"}),
    ("help", {"en": "Safety and help", "es": "Seguridad y ayuda", "gl": "Seguridade e axuda"}),
]

TEXT = {
    "en": {"manual": "OpenQ User Manual", "sub": "User manual", "chapters": "Chapters", "chapter": "Chapter",
           "contents": "Contents", "prev": "Previous", "next": "Next", "pages": "Previous and next chapter"},
    "es": {"manual": "Manual de usuario de OpenQ", "sub": "Manual de usuario", "chapters": "Capítulos", "chapter": "Capítulo",
           "contents": "Contenido", "prev": "Anterior", "next": "Siguiente", "pages": "Capítulo anterior y siguiente"},
    "gl": {"manual": "Manual de usuario de OpenQ", "sub": "Manual de usuario", "chapters": "Capítulos", "chapter": "Capítulo",
           "contents": "Contido", "prev": "Anterior", "next": "Seguinte", "pages": "Capítulo anterior e seguinte"},
}

CONTENT = re.compile(r"<!--content-->\n(.*?)<!--/content-->", re.S)
SECTION = re.compile(r'<section id="([\w-]+)">(.*?)</section>', re.S)
H2 = re.compile(r'(<h2>)(?:<span class="n">[^<]*</span>)?(.*?)(</h2>)', re.S)
H3 = re.compile(r'<h3( id="[\w-]+")?>(.*?)</h3>', re.S)
ID = re.compile(r'\bid="([\w-]+)"')
LINK = re.compile(r'href="(?:([\w-]+?)(?:\.(?:es|gl))?\.html)?#([\w-]+)"')


def file_name(stem, lang):
    return f"{stem}.html" if lang == "en" else f"{stem}.{lang}.html"


def plain(fragment):
    """Heading text without tags or icons, for the rail and the contents."""
    text = re.sub(r'<span class="ms"[^>]*>.*?</span>', "", fragment)
    return re.sub(r"\s+", " ", re.sub(r"<[^>]+>", "", text)).strip()


def slug(fragment):
    return re.sub(r"[^a-z0-9]+", "-", html.unescape(plain(fragment)).lower()).strip("-")


def read_content(stem, lang):
    path = DIR / file_name(stem, lang)
    m = CONTENT.search(path.read_text(encoding="utf-8"))
    if not m:
        sys.exit(f"{path.name}: no <!--content--> ... <!--/content--> block")
    return m.group(1)


def outline(content):
    """[(section id, h2 text, [(h3 id, h3 text)])] of a page's content."""
    out = []
    for sid, body in SECTION.findall(content):
        h2 = H2.search(body)
        subs = [(m.group(1)[5:-1], plain(m.group(2))) for m in H3.finditer(body)]
        out.append((sid, plain(h2.group(2)) if h2 else sid, subs))
    return out


def give_ids(content, ids):
    """Put an id on every <h3> that has none: the English page's ids by position (`ids`), or new slugs when `ids` is None."""
    taken = set(ID.findall(content))
    made = []

    def one(m):
        if m.group(1):
            made.append(m.group(1)[5:-1])
            return m.group(0)
        if ids is None:
            base = new = slug(m.group(2)) or "part"
            n = 2
            while new in taken:
                new, n = f"{base}-{n}", n + 1
        else:
            new = ids[len(made)]
        taken.add(new)
        made.append(new)
        return f'<h3 id="{new}">{m.group(2)}</h3>'

    content = H3.sub(one, content)
    return content, made


def number(content, chapter):
    n = iter(range(1, 1000))
    return H2.sub(lambda m: f'{m.group(1)}<span class="n">{chapter}.{next(n)}</span>{m.group(2).strip()}{m.group(3)}', content)


def relink(content, stem, lang, where):
    def one(m):
        target = m.group(2)
        page = where.get(target)
        if page is None:
            sys.exit(f"{file_name(stem, lang)}: link to #{target}, which no page has")
        return f'href="#{target}"' if page == stem else f'href="{file_name(page, lang)}#{target}"'

    return LINK.sub(one, content)


def head(lang, title):
    return f"""<!doctype html>
<html lang="{lang}">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover">
<!-- Built by tools/build_manual.py around the content block below: edit only that, then run the script. -->
<title>{title}</title>
<link rel="preconnect" href="https://fonts.googleapis.com">
<link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
<link rel="stylesheet" href="https://fonts.googleapis.com/css2?family=Barlow+Condensed:wght@500;600;700&family=Inter:wght@400;500;600&family=JetBrains+Mono:wght@400;500&display=swap">
<link rel="stylesheet" href="manual.css">
</head>
<body>
<div class="wrap">
"""


def rail(lang, stem, pages):
    t = TEXT[lang]
    langs = "".join(
        f'<a href="{file_name(stem, c)}" hreflang="{c}" title="{LANG_NAMES[c][1]}"{" aria-current=\"page\"" if c == lang else ""}>{LANG_NAMES[c][0]}</a>'
        for c in LANGS)
    items = []
    for s, titles in CHAPTERS:
        here = s == stem
        a = f'<a href="{file_name(s, lang)}"{" aria-current=\"page\"" if here else ""}>{titles[lang]}</a>'
        if not here:
            items.append(f"    <li>{a}</li>")
            continue
        sub = []
        for sid, h2, h3s in pages[s][lang]:
            sub.append(f'      <li class="s2"><a href="#{sid}">{h2}</a></li>')
            sub += [f'      <li class="s3"><a href="#{hid}">{h3}</a></li>' for hid, h3 in h3s]
        items.append(f'    <li class="cur">{a}\n     <ol class="sub">\n' + "\n".join(sub) + "\n     </ol>\n    </li>")
    return f"""<nav class="toc" aria-label="{t["chapters"]}">
  <div class="brand"><a href="{file_name("index", lang)}" title="{t["contents"]}">Open<b>Q</b></a></div>
  <div class="brand-sub">{t["sub"]}</div>
  <div class="langs">{langs}</div>
  <ol class="chapters">
""" + "\n".join(items) + "\n  </ol>\n</nav>\n\n<main>\n"


def pager(lang, i):
    t = TEXT[lang]
    prev = CHAPTERS[i - 1] if i > 0 else ("index", {lang: t["contents"]})
    nxt = CHAPTERS[i + 1] if i + 1 < len(CHAPTERS) else None
    out = f'<nav class="pager" aria-label="{t["pages"]}">\n'
    out += f'  <a class="prev" href="{file_name(prev[0], lang)}"><span>← {t["prev"]}</span>{prev[1][lang]}</a>\n'
    if nxt:
        out += f'  <a class="next" href="{file_name(nxt[0], lang)}"><span>{t["next"]} →</span>{nxt[1][lang]}</a>\n'
    return out + "</nav>\n"


def contents(lang, pages):
    t = TEXT[lang]
    cards = []
    for i, (s, titles) in enumerate(CHAPTERS, 1):
        page = file_name(s, lang)
        rows = []
        for sid, h2, h3s in pages[s][lang]:
            subs = " · ".join(f'<a href="{page}#{hid}">{h3}</a>' for hid, h3 in h3s)
            rows.append(f'      <li><a class="sec" href="{page}#{sid}">{h2}</a>' + (f'<span class="subs">{subs}</span>' if subs else "") + "</li>")
        cards.append(f'  <div class="chapter-card">\n    <a class="ct" href="{page}"><span class="n">{i}</span>{titles[lang]}</a>\n'
                     f'    <ul>\n' + "\n".join(rows) + "\n    </ul>\n  </div>")
    return f'<section id="contents">\n  <h2>{t["contents"]}</h2>\n<div class="chapter-cards">\n' + "\n".join(cards) + "\n</div>\n</section>\n"


TAIL = "</main>\n</div>\n<script src=\"manual.js\"></script>\n</body>\n</html>\n"


def main():
    stems = [s for s, _ in CHAPTERS]
    raw = {s: {lang: read_content(s, lang) for lang in LANGS} for s in stems}
    # Ids: English first, then the same ids by position in the translations.
    ids = {}
    for s in stems:
        raw[s]["en"], ids[s] = give_ids(raw[s]["en"], None)
        en_sections = [sid for sid, _ in SECTION.findall(raw[s]["en"])]
        for lang in LANGS[1:]:
            secs = [sid for sid, _ in SECTION.findall(raw[s][lang])]
            if secs != en_sections:
                sys.exit(f"{file_name(s, lang)}: sections {secs} differ from the English page's {en_sections}")
            if len(H3.findall(raw[s][lang])) != len(ids[s]):
                sys.exit(f"{file_name(s, lang)}: {len(H3.findall(raw[s][lang]))} <h3>s, the English page has {len(ids[s])}")
            raw[s][lang], made = give_ids(raw[s][lang], ids[s])
            if made != ids[s]:
                sys.exit(f"{file_name(s, lang)}: <h3> ids {made} differ from the English page's {ids[s]}")
    where = {}
    for s in stems:
        for i in ID.findall(raw[s]["en"]):
            if i in where:
                sys.exit(f"id {i} is on both {where[i]} and {s}")
            where[i] = s
    pages = {}
    for n, s in enumerate(stems, 1):
        pages[s] = {}
        for lang in LANGS:
            raw[s][lang] = relink(number(raw[s][lang], n), s, lang, where)
            pages[s][lang] = outline(raw[s][lang])
    written = 0
    for i, (s, titles) in enumerate(CHAPTERS):
        for lang in LANGS:
            t = TEXT[lang]
            out = (head(lang, f"{titles[lang]} · {t['manual']}") + rail(lang, s, pages)
                   + f'<header class="chapter-head">\n  <div class="eyebrow">{t["chapter"]} {i + 1} · {t["manual"]}</div>\n'
                   + f"  <h1>{titles[lang]}</h1>\n</header>\n\n"
                   + "<!--content-->\n" + raw[s][lang] + "<!--/content-->\n\n" + pager(lang, i) + TAIL)
            written += write(file_name(s, lang), out)
    for lang in LANGS:
        hero = relink(read_content("index", lang), "index", lang, where)
        out = (head(lang, TEXT[lang]["manual"]) + rail(lang, "index", pages)
               + "<!--content-->\n" + hero + "<!--/content-->\n\n" + contents(lang, pages) + TAIL)
        written += write(file_name("index", lang), out)
    print(f"{written} page(s) changed")


def write(name, text):
    path = DIR / name
    if path.exists() and path.read_text(encoding="utf-8") == text:
        return 0
    path.write_text(text, encoding="utf-8")
    return 1


if __name__ == "__main__":
    main()
