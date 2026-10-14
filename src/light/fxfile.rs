//! Effect files (`.oqfx`): one effect as JSON, to share effects between shows and users.
//!
//! A file holds the effect's parameters and, for custom effects, the generated table and the
//! script that made it. Files are plain data: reading one never runs its script (that only
//! happens in the sandbox when someone presses Run). Everything read is checked and clamped by
//! [`sanitize`], which also checks what scripts return.

use super::effects::{Effect, Shape};
use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};

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

/// The file for an effect.
pub fn write(effect: &Effect, author: &str, description: &str) -> String {
    let mut effect = effect.clone();
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
    f.effect = sanitize(f.effect)?;
    f.author = clip(&f.author, 200);
    f.description = clip(&f.description, 4000);
    Ok(f)
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
        let f = read(&text).unwrap();
        assert_eq!(f.effect, Effect { id: 0, ..custom() });
        assert_eq!((f.author.as_str(), f.description.as_str()), ("Me", "A two-step chase"));
        let plain = Effect { shape: Shape::Square, ..Default::default() };
        assert_eq!(read(&write(&plain, "", "")).unwrap().effect.shape, Shape::Square, "waveform effects too");
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
