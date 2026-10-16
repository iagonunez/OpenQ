//! Effect files (`.oqfx`): one effect as JSON, to share effects between shows and users.
//!
//! A file holds the effect's parameters and, for custom effects, the generated table: plain data,
//! never code. The Python script that made a table stays in the show it was written in; it is
//! left out when an effect is written to a file ([`write()`]) and dropped from anything read
//! ([`read`]), older files included. Scripts only run in the sandbox, from the script editor.
//! Everything read is checked and clamped by [`sanitize`], which also checks what scripts return.
//!
//! The effect library is the folder `<config>/effects` ([`EffectLibrary`]): effects saved there
//! are offered in every show.

use super::effects::{Effect, Shape};
use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// File extension (without the dot).
pub const EXTENSION: &str = "oqfx";
/// Format version written; files with a newer one are refused.
pub const FORMAT: u32 = 1;
/// Largest file read.
pub const MAX_FILE: usize = 32 << 20;
/// Longest script kept.
pub const MAX_SCRIPT: usize = 256 << 10;

/// The contents of an effect file.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct EffectFile {
    /// Format version ([`FORMAT`]).
    pub openq_effect: u32,
    /// Who made it.
    pub author: String,
    /// What it does.
    pub description: String,
    /// The effect (its number is ignored on import).
    pub effect: Effect,
}

impl Default for EffectFile {
    /// Format 0: a JSON document without the format field isn't an effect file.
    fn default() -> Self {
        Self { openq_effect: 0, author: String::new(), description: String::new(), effect: Effect::default() }
    }
}

/// The file for an effect: its data without the script (and the script's settings).
pub fn write(effect: &Effect, author: &str, description: &str) -> String {
    let mut effect = without_script(effect.clone());
    effect.id = 0;
    let f = EffectFile { openq_effect: FORMAT, author: author.into(), description: description.into(), effect };
    serde_json::to_string_pretty(&f).expect("effects serialize")
}

/// Read and check an effect file.
pub fn read(text: &str) -> anyhow::Result<EffectFile> {
    if text.len() > MAX_FILE {
        bail!("the file is larger than {} MB", MAX_FILE >> 20);
    }
    let mut f: EffectFile = serde_json::from_str(text).context("not an OpenQ effect file")?;
    if f.openq_effect == 0 {
        bail!("not an OpenQ effect file");
    }
    if f.openq_effect > FORMAT {
        bail!("made by a newer OpenQ (format {}); update OpenQ to read it", f.openq_effect);
    }
    f.effect = without_script(sanitize(f.effect)?);
    f.author = clip(&f.author, 200);
    f.description = clip(&f.description, 4000);
    Ok(f)
}

/// An effect as data only: no script, and none of the script's settings (they only feed a run).
pub fn without_script(mut e: Effect) -> Effect {
    e.script = String::new();
    e.params = Vec::new();
    e
}

/// Check an effect from outside (a file or a script): the table must be valid, parameters are
/// clamped to what the editor allows, texts are shortened. The number is reset to 0.
pub fn sanitize(mut e: Effect) -> anyhow::Result<Effect> {
    e.id = 0;
    e.name = clip(&e.name, 200);
    if e.script.len() > MAX_SCRIPT {
        bail!("the script is longer than {} KB", MAX_SCRIPT >> 10);
    }
    let finite = |v: f32, d: f32| if v.is_finite() { v } else { d };
    e.speed = finite(e.speed, 0.5).clamp(0.01, 50.0);
    e.size = finite(e.size, 1.0).clamp(0.0, 1.0);
    e.base = finite(e.base, 0.0).clamp(0.0, 1.0);
    e.duty = finite(e.duty, 0.5).clamp(0.0, 1.0);
    e.spread = finite(e.spread, 0.0).clamp(0.0, 1440.0);
    e.groups = e.groups.clamp(1, 64);
    e.wings = e.wings.clamp(1, 16);
    e.params = super::effects::sanitize_params(std::mem::take(&mut e.params));
    if let Some(a) = &mut e.audio {
        a.gain = finite(a.gain, 1.0).clamp(0.0, 100.0);
        a.floor = finite(a.floor, 0.0).clamp(0.0, 1.0);
    }
    match (&e.table, e.shape) {
        (Some(t), Shape::Custom) => {
            t.validate().map_err(|m| anyhow::anyhow!("invalid effect table: {m}"))?;
            e.attribute = t.attributes[0];
        }
        (None, Shape::Custom) => bail!("a custom effect without a table"),
        (Some(_), _) => e.table = None,
        (None, _) => {}
    }
    Ok(e)
}

fn clip(s: &str, max: usize) -> String {
    s.chars().filter(|c| !c.is_control() || *c == '\n').take(max).collect()
}

/// The user's effect library folder (`<config>/effects`).
pub fn dir() -> Option<PathBuf> {
    crate::remote::config_dir().map(|d| d.join("effects"))
}

/// An effect file in the library.
#[derive(Clone, Debug, PartialEq)]
pub struct LibraryEffect {
    /// File name in the folder.
    pub file: String,
    /// What the file holds.
    pub contents: EffectFile,
}

/// File name for an effect: its name with anything unusual replaced (`effect` when unnamed).
pub fn file_name(e: &Effect) -> String {
    let stem: String = e.name.trim().chars().take(80).map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    format!("{}.{EXTENSION}", if stem.is_empty() { "effect" } else { &stem })
}

impl super::folder::Stored for LibraryEffect {
    const THREAD: &'static str = "openq-effects";
    /// Every `.oqfx` in the folder, by effect name.
    fn load_dir(dir: &Path) -> (Vec<Self>, Vec<(String, String)>) {
        let (mut items, mut errors) = (Vec::new(), Vec::new());
        let Ok(rd) = std::fs::read_dir(dir) else { return (items, errors) };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some(EXTENSION) {
                continue;
            }
            let file = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            let r = std::fs::metadata(&p)
                .map_err(anyhow::Error::from)
                .and_then(|m| if m.len() as usize > MAX_FILE { Err(anyhow::anyhow!("too big")) } else { Ok(()) })
                .and_then(|_| std::fs::read_to_string(&p).map_err(anyhow::Error::from))
                .and_then(|s| read(&s));
            match r {
                Ok(contents) => items.push(LibraryEffect { file, contents }),
                Err(e) => errors.push((file, format!("{e:#}"))),
            }
        }
        items.sort_by_key(|x| (x.contents.effect.name.to_lowercase(), x.file.clone()));
        (items, errors)
    }
}

/// The effect library ([`super::folder::Library`] of effect files).
pub type EffectLibrary = super::folder::Library<LibraryEffect>;

impl Default for EffectLibrary {
    fn default() -> Self {
        Self::new(dir())
    }
}

impl EffectLibrary {
    /// Save an effect into the folder (replacing a file of the same name), then reload.
    pub fn save(&self, effect: Effect, author: String, description: String, done: impl FnOnce(anyhow::Result<()>) + Send + 'static) {
        self.job(
            move |dir| {
                std::fs::create_dir_all(dir)?;
                crate::recovery::write_atomic(&dir.join(file_name(&effect)), write(&effect, &author, &description).as_bytes())
            },
            done,
        );
    }

    /// Remove a file from the folder, then reload.
    pub fn remove(&self, file: String, done: impl FnOnce(anyhow::Result<()>) + Send + 'static) {
        self.job(
            move |dir| {
                // Only plain names inside the folder.
                anyhow::ensure!(!file.contains(['/', '\\']) && file.ends_with(EXTENSION), "not a library file");
                std::fs::remove_file(dir.join(&file))?;
                Ok(())
            },
            done,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::light::effects::EffectTable;
    use crate::light::Attribute;

    fn custom() -> Effect {
        Effect {
            id: 7,
            name: "Chase".into(),
            shape: Shape::Custom,
            table: Some(std::sync::Arc::new(EffectTable { attributes: vec![Attribute::Dimmer], steps: 2, width: 2, smooth: false, values: vec![1.0, 0.0, 0.0, 1.0] })),
            script: "import openq\n".into(),
            ..Default::default()
        }
    }

    #[test]
    fn roundtrip() {
        let text = write(&custom(), "Me", "A two-step chase");
        assert!(!text.contains("import openq"), "files hold no script");
        let f = read(&text).unwrap();
        assert_eq!(f.effect, Effect { id: 0, script: String::new(), ..custom() });
        assert_eq!((f.author.as_str(), f.description.as_str()), ("Me", "A two-step chase"));
        let plain = Effect { shape: Shape::Square, ..Default::default() };
        assert_eq!(read(&write(&plain, "", "")).unwrap().effect.shape, Shape::Square, "waveform effects too");
    }

    #[test]
    fn library_saves_lists_and_removes() {
        let dir = std::env::temp_dir().join(format!("openq-fxlib-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let lib = EffectLibrary::new(Some(dir.clone()));
        lib.wait();
        assert!(lib.snapshot().items.is_empty());
        let (tx, rx) = std::sync::mpsc::channel();
        let tx2 = tx.clone();
        lib.save(custom(), "me".into(), "a chase".into(), move |r| tx.send(r.is_ok()).unwrap());
        lib.save(Effect { name: "../Wave".into(), ..Default::default() }, String::new(), String::new(), move |r| tx2.send(r.is_ok()).unwrap());
        assert!(rx.recv().unwrap() && rx.recv().unwrap());
        lib.wait();
        let snap = lib.snapshot();
        let files: Vec<&str> = snap.items.iter().map(|x| x.file.as_str()).collect();
        assert_eq!(files, ["___Wave.oqfx", "Chase.oqfx"], "sorted by effect name; no path in the file name");
        assert_eq!(snap.items[1].contents.description, "a chase");
        assert_eq!(snap.items[1].contents.effect.table, custom().table);
        std::fs::write(dir.join("bad.oqfx"), "{").unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        lib.remove("Chase.oqfx".into(), move |r| tx.send(r.is_ok()).unwrap());
        assert!(rx.recv().unwrap());
        let (tx, rx) = std::sync::mpsc::channel();
        lib.remove("../x.oqfx".into(), move |r| tx.send(r.is_ok()).unwrap());
        assert!(!rx.recv().unwrap(), "no paths");
        lib.wait();
        let snap = lib.snapshot();
        assert_eq!(snap.items.len(), 1);
        assert_eq!(snap.errors.len(), 1, "the damaged file is reported");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A file with a script (an older one, or made by hand) is read as data: the script is
    /// dropped, never kept to run later.
    #[test]
    fn scripts_in_files_are_dropped() {
        let mut f = EffectFile { openq_effect: FORMAT, author: String::new(), description: String::new(), effect: custom() };
        f.effect.params = vec![super::super::effects::ScriptParam { name: "n".into(), min: 0.0, max: 1.0, value: 0.5, integer: false }];
        let read = read(&serde_json::to_string(&f).unwrap()).unwrap();
        assert!(read.effect.script.is_empty() && read.effect.params.is_empty());
        assert_eq!(read.effect.table, custom().table, "the generated table stays");
    }

    #[test]
    fn hostile_files_are_refused_or_clamped() {
        assert!(read("{}").is_err(), "no format field");
        assert!(read("[1, 2]").is_err());
        assert!(read(r#"{"openq_effect": 99, "effect": {}}"#).unwrap_err().to_string().contains("newer"));
        let mut e = custom();
        std::sync::Arc::make_mut(e.table.as_mut().unwrap()).values.push(0.5);
        assert!(read(&write(&e, "", "")).is_err(), "table size");
        let mut e = custom();
        e.table = None;
        assert!(read(&write(&e, "", "")).is_err(), "custom without a table");
        let text = r#"{"openq_effect": 1, "effect": {"name": "x\u0007y", "speed": 1e9, "groups": 100000, "spread": -5,
            "shape": "Custom", "table": {"attributes": ["Red"], "steps": 1, "width": 1, "values": [0.5]}, "attribute": "Pan"}}"#;
        let f = read(text).unwrap();
        assert_eq!((f.effect.name.as_str(), f.effect.speed, f.effect.groups, f.effect.spread), ("xy", 50.0, 64, 0.0));
        assert_eq!(f.effect.attribute, Attribute::Red, "attribute follows the table");
        let big = format!(r#"{{"openq_effect": 1, "effect": {{"script": "{}"}}}}"#, "a".repeat(MAX_SCRIPT + 1));
        assert!(read(&big).is_err());
    }
}
