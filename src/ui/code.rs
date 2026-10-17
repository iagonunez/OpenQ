//! Syntax highlighting for the effect formula editor ([`crate::light::formula`]): a small tokenizer
//! that colours keywords, functions and variables, attribute names, texts, numbers and comments,
//! laid out as one egui `LayoutJob`.

use super::theme;
use crate::light::formula;
use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, FontId};

/// What a span of source is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Kind {
    Plain,
    Keyword,
    /// Functions and the built-in variables.
    Builtin,
    /// Attribute names.
    Attribute,
    Str,
    Number,
    Comment,
}

const KEYWORDS: &[&str] = &["effect", "smooth", "param", "int", "let", "and", "or", "not"];
const VARIABLES: &[&str] = &["t", "step", "steps", "row", "width", "hsv"];

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
        if c == b'"' {
            i += 1;
            while i < b.len() && b[i] != b'"' && b[i] != b'\n' {
                i += 1;
            }
            if i < b.len() && b[i] == b'"' {
                i += 1;
            }
            push(start..i, Kind::Str);
            continue;
        }
        if c.is_ascii_digit() || (c == b'.' && b.get(i + 1).is_some_and(|d| d.is_ascii_digit())) {
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            if i + 1 < b.len() && b[i] == b'.' && b[i + 1].is_ascii_digit() {
                i += 1;
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }
            }
            push(start..i, Kind::Number);
            continue;
        }
        if c.is_ascii_alphabetic() || c == b'_' {
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            let word = src[start..i].to_ascii_lowercase();
            // `effect` heads an effect only with its name in quotes; otherwise it's the attribute.
            let header = word == "effect" && src[i..].trim_start().starts_with('"');
            // `steps` and `width` are keywords on an effect line, variables elsewhere.
            let on_effect_line = src[..start].rsplit('\n').next().is_some_and(|l| l.trim_start().to_ascii_lowercase().starts_with("effect") && l.contains('"'));
            let size_word = (word == "steps" || word == "width") && on_effect_line;
            let kind = if (KEYWORDS.contains(&word.as_str()) && word != "effect") || header || size_word {
                Kind::Keyword
            } else if VARIABLES.contains(&word.as_str()) || formula::Func::ALL.iter().any(|f| f.0 == word) {
                Kind::Builtin
            } else if formula::attribute(&word).is_some() {
                Kind::Attribute
            } else {
                Kind::Plain
            };
            push(start..i, kind);
            continue;
        }
        // Anything else: one character (UTF-8 safe).
        i += src[i..].chars().next().map_or(1, char::len_utf8);
        push(start..i, Kind::Plain);
    }
    out
}

fn colour(k: Kind) -> Color32 {
    match k {
        Kind::Plain => theme::TEXT,
        Kind::Keyword => Color32::from_rgb(0xc5, 0x86, 0xf0),
        Kind::Builtin => Color32::from_rgb(0x5c, 0xc8, 0xd8),
        Kind::Attribute => Color32::from_rgb(0x6c, 0xa8, 0xff),
        Kind::Str => Color32::from_rgb(0x9c, 0xd0, 0x7a),
        Kind::Number => Color32::from_rgb(0xf0, 0xa8, 0x60),
        Kind::Comment => Color32::from_gray(0x78),
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
    fn formula_tokens() {
        let src = "param lit 2 1..8 int  # the API\neffect \"Wave\" steps 16 width 2 smooth\nlet k = sine(t + row / width)\nred, effect = hsv(k), 0.5";
        let k = kinds(src);
        for want in [("param", Kind::Keyword), ("int", Kind::Keyword), ("effect", Kind::Keyword), ("smooth", Kind::Keyword), ("let", Kind::Keyword)] {
            assert!(k.contains(&want), "{want:?}");
        }
        assert!(k.contains(&("# the API", Kind::Comment)));
        assert!(k.contains(&("\"Wave\"", Kind::Str)));
        assert!(k.contains(&("16", Kind::Number)) && k.contains(&("0.5", Kind::Number)) && k.contains(&("1", Kind::Number)));
        assert!(k.contains(&("sine", Kind::Builtin)) && k.contains(&("t", Kind::Builtin)) && k.contains(&("hsv", Kind::Builtin)));
        assert!(k.contains(&("steps", Kind::Keyword)) && k.contains(&("width", Kind::Keyword)) && k.contains(&("width", Kind::Builtin)));
        assert!(k.contains(&("red", Kind::Attribute)) && k.contains(&("effect", Kind::Attribute)), "effect without a name is the attribute");
    }

    #[test]
    fn spans_cover_everything() {
        for src in ["red = \"unterminated\n1 + .5 # c", "ñ = 1..2", "x", ""] {
            let t = tokens(src);
            let mut at = 0;
            for (r, _) in &t {
                assert_eq!(r.start, at, "{src:?}");
                at = r.end;
            }
            assert_eq!(at, src.len(), "{src:?}");
        }
    }
}
