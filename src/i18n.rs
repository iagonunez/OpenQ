//! UI translations. Texts are written in English in the code and looked up by that English text
//! in `assets/i18n/<code>.json` (`t("Save")`, or `tr!("Saved {path}", path = …)` for texts with
//! values). A text with no entry stays English. The language is an app setting kept in
//! `<config>/language`, chosen on the first run and in Settings → General.
//!
//! Show-control words (cue, GO, fade…) stay English in every language.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::OnceLock;

/// A UI language.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    /// English (the texts in the code).
    En,
    /// Spanish.
    Es,
    /// Galician.
    Gl,
}

impl Lang {
    /// Every language, in the order the pickers list them.
    pub const ALL: [Lang; 3] = [Lang::En, Lang::Es, Lang::Gl];

    /// ISO 639-1 code, as saved in the config file.
    pub fn code(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::Es => "es",
            Lang::Gl => "gl",
        }
    }

    /// The language's name in itself.
    pub fn name(self) -> &'static str {
        match self {
            Lang::En => "English",
            Lang::Es => "Español",
            Lang::Gl => "Galego",
        }
    }

    /// Parses a code or a locale such as `gl_ES.UTF-8`.
    pub fn from_code(s: &str) -> Option<Lang> {
        let s = s.trim().to_ascii_lowercase();
        Lang::ALL.into_iter().find(|l| s == l.code() || s.starts_with(&format!("{}_", l.code())) || s.starts_with(&format!("{}-", l.code())))
    }

    /// The system's language, from `LC_ALL`/`LC_MESSAGES`/`LANG` (English if it isn't one of ours).
    pub fn system() -> Lang {
        ["LC_ALL", "LC_MESSAGES", "LANG"]
            .iter()
            .filter_map(|v| std::env::var(v).ok())
            .find(|v| !v.is_empty())
            .and_then(|v| Lang::from_code(&v))
            .unwrap_or(Lang::En)
    }

    fn table(self) -> Option<&'static HashMap<String, String>> {
        static ES: OnceLock<HashMap<String, String>> = OnceLock::new();
        static GL: OnceLock<HashMap<String, String>> = OnceLock::new();
        let (cell, src) = match self {
            Lang::En => return None,
            Lang::Es => (&ES, include_str!("../assets/i18n/es.json")),
            Lang::Gl => (&GL, include_str!("../assets/i18n/gl.json")),
        };
        Some(cell.get_or_init(|| serde_json::from_str(src).expect("bad translation table")))
    }
}

static LANG: AtomicU8 = AtomicU8::new(0);

/// The current UI language.
pub fn lang() -> Lang {
    Lang::ALL[LANG.load(Ordering::Relaxed) as usize]
}

/// Switches the UI language (from the next frame on). Doesn't save it: see [`save`].
pub fn set_lang(l: Lang) {
    LANG.store(Lang::ALL.iter().position(|x| *x == l).unwrap_or(0) as u8, Ordering::Relaxed);
}

fn config_file() -> Option<std::path::PathBuf> {
    crate::remote::config_dir().map(|d| d.join("language"))
}

/// Loads the saved language and makes it current. `OPENQ_LANG` overrides it. Returns false when
/// none was ever chosen (first run), so the UI can ask.
pub fn init() -> bool {
    if let Some(l) = std::env::var("OPENQ_LANG").ok().and_then(|v| Lang::from_code(&v)) {
        set_lang(l);
        return true;
    }
    match config_file().and_then(|p| crate::recovery::read_text(&p, 1024).ok()).and_then(|s| Lang::from_code(&s)) {
        Some(l) => {
            set_lang(l);
            true
        }
        None => {
            set_lang(Lang::system());
            false
        }
    }
}

/// Saves the current language as the user's choice.
pub fn save() -> anyhow::Result<()> {
    let path = config_file().ok_or_else(|| anyhow::anyhow!("no config folder"))?;
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(path, lang().code())?;
    Ok(())
}

/// Translates an English UI text into the current language.
pub fn t(s: &str) -> &str {
    lookup(lang(), s).unwrap_or(s)
}

/// The translation of `s` in `l`, if the table has one.
pub fn lookup(l: Lang, s: &str) -> Option<&'static str> {
    l.table()?.get(s).map(|v| v.as_str())
}

/// Replaces each `{name}` in `template` by its value.
pub fn fill(template: &str, args: &[(&str, &dyn std::fmt::Display)]) -> String {
    let mut out = template.to_string();
    for (k, v) in args {
        out = out.replace(&format!("{{{k}}}"), &v.to_string());
    }
    out
}

/// Translates a text with named values: `tr!("Saved {path}", path = p.display())`, or
/// `tr!("{n} cues", n)` for a variable with the placeholder's name. Returns a `String`.
#[macro_export]
macro_rules! tr {
    ($s:literal $(, $k:ident $(= $v:expr)?)* $(,)?) => {
        $crate::i18n::fill($crate::i18n::t($s), &[$((stringify!($k), &$crate::tr!(@v $k $(= $v)?) as &dyn ::std::fmt::Display)),*])
    };
    (@v $k:ident) => { $k };
    (@v $k:ident = $v:expr) => { $v };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// String literals passed to `t(…)`/`tr!(…)` in the sources.
    fn keys_in_sources() -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut stack = vec![std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/src"))];
        while let Some(p) = stack.pop() {
            if p.is_dir() {
                stack.extend(std::fs::read_dir(&p).unwrap().map(|e| e.unwrap().path()));
                continue;
            }
            if p.extension().is_none_or(|e| e != "rs") || p.ends_with("i18n.rs") {
                continue;
            }
            let src = std::fs::read_to_string(&p).unwrap();
            for start in ["t(", "tr!("] {
                let mut from = 0;
                while let Some(i) = src[from..].find(start) {
                    let at = from + i;
                    from = at + start.len();
                    // `t(` must be a call of its own, not the end of another name.
                    if at > 0 {
                        let b = src.as_bytes()[at - 1];
                        if b.is_ascii_alphanumeric() || b == b'_' {
                            continue;
                        }
                    }
                    let Some(rest) = src[from..].trim_start().strip_prefix('"') else { continue };
                    out.push((parse_literal(rest), format!("{}", p.display())));
                }
            }
        }
        out
    }

    /// The value of a Rust string literal whose opening quote was just consumed.
    fn parse_literal(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            match c {
                '"' => break,
                '\\' => match chars.next().unwrap() {
                    'n' => out.push('\n'),
                    't' => out.push('\t'),
                    '\n' => {
                        // Line continuation: skip the next line's leading whitespace.
                        let rest: String = chars.clone().collect();
                        let trimmed = rest.trim_start();
                        let skip = rest.chars().count() - trimmed.chars().count();
                        for _ in 0..skip {
                            chars.next();
                        }
                    }
                    'u' => {
                        let hex: String = chars.by_ref().skip(1).take_while(|c| *c != '}').collect();
                        out.push(char::from_u32(u32::from_str_radix(&hex, 16).unwrap()).unwrap());
                    }
                    c => out.push(c),
                },
                c => out.push(c),
            }
        }
        out
    }

    /// Texts translated through names in the model (not literals at the call site).
    fn model_keys() -> Vec<String> {
        crate::ui::translated_names().into_iter().map(String::from).collect()
    }

    #[test]
    fn every_text_is_translated() {
        let mut missing = Vec::new();
        let keys = keys_in_sources();
        assert!(keys.len() > 300, "the source scan found only {} texts", keys.len());
        let all = keys.into_iter().chain(model_keys().into_iter().map(|k| (k, "model".to_string())));
        for (key, file) in all {
            for l in [Lang::Es, Lang::Gl] {
                if lookup(l, &key).is_none() {
                    missing.push(format!("{} {file}: {key:?}", l.code()));
                }
            }
        }
        missing.sort();
        missing.dedup();
        assert!(missing.is_empty(), "{} untranslated texts:\n{}", missing.len(), missing.join("\n"));
    }

    #[test]
    fn placeholders_match() {
        for l in [Lang::Es, Lang::Gl] {
            for (k, v) in l.table().unwrap() {
                let names = |s: &str| {
                    let mut n: Vec<String> = s.split('{').skip(1).filter_map(|p| p.split_once('}').map(|x| x.0.to_string())).collect();
                    n.sort();
                    n
                };
                assert_eq!(names(k), names(v), "{}: {k:?}", l.code());
            }
        }
    }

    #[test]
    fn fill_and_codes() {
        assert_eq!(fill("{n} of {m}", &[("n", &1), ("m", &"two")]), "1 of two");
        assert_eq!(Lang::from_code("gl_ES.UTF-8"), Some(Lang::Gl));
        assert_eq!(Lang::from_code("es"), Some(Lang::Es));
        assert_eq!(Lang::from_code("fr_FR"), None);
        let n = 3;
        assert_eq!(tr!("{n} cues", n), "3 cues"); // English: no table
    }
}
