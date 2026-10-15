//! Python syntax highlighting for the effect script editor: a small tokenizer (no parser, no
//! extra dependency) that colours keywords, built-ins, the `openq` module, strings, numbers,
//! comments, decorators and the names after `def`/`class`, laid out as one egui `LayoutJob`.

use super::theme;
use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, FontId};

/// What a span of source is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Kind {
    Plain,
    Keyword,
    Builtin,
    /// `openq` and what follows `openq.`.
    Api,
    Str,
    Number,
    Comment,
    Decorator,
    /// The name being defined after `def` / `class`.
    Definition,
}

const KEYWORDS: &[&str] = &[
    "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del", "elif", "else", "except",
    "finally", "for", "from", "global", "if", "import", "in", "is", "lambda", "match", "case", "nonlocal", "not", "or", "pass", "raise",
    "return", "try", "while", "with", "yield",
];

const BUILTINS: &[&str] = &[
    "abs", "all", "any", "bool", "dict", "divmod", "enumerate", "filter", "float", "int", "len", "list", "map", "max", "min", "pow",
    "print", "range", "reversed", "round", "set", "sorted", "str", "sum", "tuple", "zip", "math", "random", "self",
];

/// Split `src` into (byte range, kind) spans covering all of it.
pub(crate) fn tokens(src: &str) -> Vec<(std::ops::Range<usize>, Kind)> {
    let b = src.as_bytes();
    let mut out: Vec<(std::ops::Range<usize>, Kind)> = Vec::new();
    let mut push = |r: std::ops::Range<usize>, k: Kind| {
        if let Some(last) = out.last_mut()
            && last.1 == k
            && last.0.end == r.start
        {
            last.0.end = r.end;
        } else if !r.is_empty() {
            out.push((r, k));
        }
    };
    let mut i = 0;
    // The previous word, for `def name` and `openq.name`.
    let mut prev_word: &str = "";
    let mut after_dot_of_api = false;
    while i < b.len() {
        let c = b[i];
        let start = i;
        if c == b'#' {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            push(start..i, Kind::Comment);
            continue;
        }
        if c == b'\'' || c == b'"' {
            let triple = b.len() >= i + 3 && b[i + 1] == c && b[i + 2] == c;
            i += if triple { 3 } else { 1 };
            while i < b.len() {
                if b[i] == b'\\' {
                    i += 2;
                    continue;
                }
                if triple {
                    if b.len() >= i + 3 && b[i] == c && b[i + 1] == c && b[i + 2] == c {
                        i += 3;
                        break;
                    }
                } else if b[i] == c || b[i] == b'\n' {
                    i += 1;
                    break;
                }
                i += 1;
            }
            i = i.min(b.len());
            push(start..i, Kind::Str);
            prev_word = "";
            continue;
        }
        if c == b'@' && src[..i].rsplit('\n').next().is_some_and(|line| line.trim().is_empty()) {
            i += 1;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'.') {
                i += 1;
            }
            push(start..i, Kind::Decorator);
            continue;
        }
        if c.is_ascii_digit() || (c == b'.' && b.get(i + 1).is_some_and(|d| d.is_ascii_digit())) {
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'.' || b[i] == b'_') {
                i += 1;
            }
            push(start..i, Kind::Number);
            prev_word = "";
            continue;
        }
        if c.is_ascii_alphabetic() || c == b'_' || c >= 0x80 {
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] >= 0x80) {
                i += 1;
            }
            let word = &src[start..i];
            let kind = if prev_word == "def" || prev_word == "class" {
                Kind::Definition
            } else if after_dot_of_api || word == "openq" {
                Kind::Api
            } else if KEYWORDS.contains(&word) {
                Kind::Keyword
            } else if BUILTINS.contains(&word) {
                Kind::Builtin
            } else {
                Kind::Plain
            };
            after_dot_of_api = false;
            if word == "openq" && b.get(i) == Some(&b'.') {
                after_dot_of_api = true;
            }
            prev_word = word;
            push(start..i, kind);
            continue;
        }
        // Anything else: one character (UTF-8 safe).
        let len = src[i..].chars().next().map_or(1, char::len_utf8);
        i += len;
        if c != b'.' && !c.is_ascii_whitespace() {
            after_dot_of_api = false;
        }
        if !c.is_ascii_whitespace() {
            prev_word = "";
        }
        push(start..i, Kind::Plain);
    }
    out
}

fn colour(k: Kind) -> Color32 {
    match k {
        Kind::Plain => theme::TEXT,
        Kind::Keyword => Color32::from_rgb(0xc5, 0x86, 0xf0),
        Kind::Builtin => Color32::from_rgb(0x5c, 0xc8, 0xd8),
        Kind::Api => Color32::from_rgb(0x6c, 0xa8, 0xff),
        Kind::Str => Color32::from_rgb(0x9c, 0xd0, 0x7a),
        Kind::Number => Color32::from_rgb(0xf0, 0xa8, 0x60),
        Kind::Comment => Color32::from_gray(0x78),
        Kind::Decorator => Color32::from_rgb(0xe8, 0xc0, 0x5a),
        Kind::Definition => Color32::from_rgb(0xf0, 0xd8, 0x7a),
    }
}

/// `src` as a coloured layout job in `font`, wrapped at `wrap` points.
pub(crate) fn highlight(src: &str, font: FontId, wrap: f32) -> LayoutJob {
    let mut job = LayoutJob { text: src.to_owned(), ..Default::default() };
    job.wrap.max_width = wrap;
    for (r, k) in tokens(src) {
        let mut f = TextFormat::simple(font.clone(), colour(k));
        if k == Kind::Comment {
            f.italics = true;
        }
        job.sections.push(egui::text::LayoutSection { leading_space: 0.0, byte_range: egui::text::ByteIndex(r.start)..egui::text::ByteIndex(r.end), format: f });
    }
    job
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str) -> Vec<(&str, Kind)> {
        tokens(src).into_iter().filter(|(_, k)| *k != Kind::Plain).map(|(r, k)| (&src[r], k)).collect()
    }

    #[test]
    fn python_tokens() {
        let src = "import openq  # the API\ndef wave(n):\n    return openq.effect(\"Wave\", steps=16, x=0.5)\n";
        let k = kinds(src);
        assert!(k.contains(&("import", Kind::Keyword)));
        assert!(k.contains(&("openq", Kind::Api)));
        assert!(k.contains(&("effect", Kind::Api)));
        assert!(k.contains(&("# the API", Kind::Comment)));
        assert!(k.contains(&("wave", Kind::Definition)));
        assert!(k.contains(&("\"Wave\"", Kind::Str)));
        assert!(k.contains(&("16", Kind::Number)) && k.contains(&("0.5", Kind::Number)));
        assert!(k.contains(&("return", Kind::Keyword)));
    }

    #[test]
    fn spans_cover_everything() {
        for src in ["s = '''a\n'b'\n''' + \"x\\\"y\" # c", "@deco\nclass A: pass", "ñ = 'unterminated", "x = 1e-3 + .5", ""] {
            let t = tokens(src);
            let mut at = 0;
            for (r, _) in &t {
                assert_eq!(r.start, at, "{src:?}");
                at = r.end;
            }
            assert_eq!(at, src.len(), "{src:?}");
        }
        assert_eq!(kinds("s = '''a\n'b'\n'''"), vec![("'''a\n'b'\n'''", Kind::Str)]);
        assert_eq!(kinds("@deco\nx")[0], ("@deco", Kind::Decorator));
    }
}
