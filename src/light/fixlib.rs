//! Fixture type files (`.oqfixture`) and the user's fixture library.
//!
//! * A file holds one [`FixtureType`] as JSON: `{"format": "openq-fixture", "version": 1,
//!   "fixture": {…}}`. Shows carry copies of the types they use, so a file is only needed to
//!   reuse or share a type.
//! * The library is the folder `<config>/fixtures`: every `.oqfixture` there is offered when
//!   patching, in every show. Imports (OFL, GDTF, QLC+) and the fixture editor save there.
//! * Files come from anywhere, so every type read goes through [`sanitize`] (sizes, offsets,
//!   names) before it reaches the show.
//! * [`Library`] reads and writes the folder on its own thread, one job at a time and in order
//!   ([`super::folder`]): the UI never touches the disk in a frame.

use super::model::{ChannelDef, FixtureMode, FixtureType, MAX_CELLS, UNIVERSE_SIZE};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// File extension of fixture type files.
pub const EXTENSION: &str = "oqfixture";
const FORMAT: &str = "openq-fixture";
const VERSION: u32 = 1;
/// Most modes a type can have.
pub const MAX_MODES: usize = 64;
/// Most named ranges per channel.
pub const MAX_RANGES: usize = 256;
/// Longest name kept (characters).
const MAX_NAME: usize = 120;

#[derive(Serialize, Deserialize)]
struct File {
    format: String,
    version: u32,
    fixture: FixtureType,
}

/// How the show's copy `show` of a type differs from the library's `lib` (empty = the same), as
/// short lines for the UI: name, physical data, modes added or missing, modes whose channels,
/// cells or slots changed.
pub fn differences(show: &FixtureType, lib: &FixtureType) -> Vec<String> {
    use crate::i18n::t;
    let mut out = Vec::new();
    if (&show.manufacturer, &show.name) != (&lib.manufacturer, &lib.name) {
        out.push(t("name").to_string());
    }
    if show.physical != lib.physical {
        out.push(t("physical data").to_string());
    }
    for m in &lib.modes {
        match show.modes.iter().find(|x| x.name == m.name) {
            None => out.push(tr!("mode \"{mode}\" only in the library", mode = m.name.clone())),
            Some(x) if x.channels != m.channels => out.push(tr!("mode \"{mode}\": channels", mode = m.name.clone())),
            Some(x) if x.heads != m.heads || x.slots != m.slots => out.push(tr!("mode \"{mode}\": cells or slots", mode = m.name.clone())),
            Some(_) => {}
        }
    }
    for m in show.modes.iter().filter(|m| !lib.modes.iter().any(|x| x.name == m.name)) {
        out.push(tr!("mode \"{mode}\" only in the show", mode = m.name.clone()));
    }
    if out.is_empty() && show != lib {
        out.push(t("mode order").to_string());
    }
    out
}

/// A fixture type as file text.
pub fn write(t: &FixtureType) -> String {
    serde_json::to_string_pretty(&File { format: FORMAT.into(), version: VERSION, fixture: t.clone() }).expect("fixture type serialises")
}

/// Read a fixture type file (checked by [`sanitize`]).
pub fn read(text: &str) -> anyhow::Result<FixtureType> {
    let f: File = serde_json::from_str(text).map_err(|e| anyhow::anyhow!("not an OpenQ fixture file: {e}"))?;
    if f.format != FORMAT {
        anyhow::bail!("not an OpenQ fixture file");
    }
    if f.version > VERSION {
        anyhow::bail!("made by a newer OpenQ (fixture file version {})", f.version);
    }
    sanitize(f.fixture)
}

fn clip(s: &mut String) {
    if s.chars().count() > MAX_NAME {
        *s = s.chars().take(MAX_NAME).collect();
    }
    s.retain(|c| !c.is_control());
}

/// Check a fixture type from outside (a file, a show from someone else): limits on modes,
/// channels, cells and ranges, offsets inside a universe, names trimmed. Values are clamped.
pub fn sanitize(mut t: FixtureType) -> anyhow::Result<FixtureType> {
    clip(&mut t.id);
    clip(&mut t.name);
    clip(&mut t.manufacturer);
    if t.id.trim().is_empty() {
        anyhow::bail!("the fixture type has no id");
    }
    if t.modes.is_empty() {
        anyhow::bail!("the fixture type has no modes");
    }
    if t.modes.len() > MAX_MODES {
        anyhow::bail!("too many modes ({})", t.modes.len());
    }
    // Pictures: thumbnails only (anything else is dropped, not decoded here).
    if t.images.len() > super::gobo::MAX_IMAGES {
        anyhow::bail!("too many pictures ({})", t.images.len());
    }
    t.images.retain(|k, v| k.len() <= 1024 && v.len() <= super::gobo::MAX_IMAGE_TEXT);
    let fix_channel = |c: &mut ChannelDef| -> anyhow::Result<()> {
        if c.offset >= UNIVERSE_SIZE || c.fine.is_some_and(|f| f >= UNIVERSE_SIZE) {
            anyhow::bail!("a channel lies outside the 512 DMX slots");
        }
        if c.ranges.len() > MAX_RANGES {
            anyhow::bail!("a channel has too many ranges ({})", c.ranges.len());
        }
        clip(&mut c.name);
        if c.dmx_break >= 8 {
            anyhow::bail!("a channel is on DMX break {} (8 at most)", c.dmx_break + 1);
        }
        c.default = c.default.filter(|v| v.is_finite()).map(|v| v.clamp(0.0, 1.0));
        c.highlight = c.highlight.filter(|v| v.is_finite()).map(|v| v.clamp(0.0, 1.0));
        for r in &mut c.ranges {
            clip(&mut r.name);
            r.name = r.name.trim().to_string();
            clip(&mut r.image);
        }
        Ok(())
    };
    let check_mode = |m: &mut FixtureMode| -> anyhow::Result<()> {
        clip(&mut m.name);
        if m.heads.len() > MAX_CELLS as usize {
            anyhow::bail!("too many cells ({})", m.heads.len());
        }
        if m.channels.len() > UNIVERSE_SIZE as usize {
            anyhow::bail!("too many channels");
        }
        for c in &mut m.channels {
            fix_channel(c)?;
        }
        let mut total = m.channels.len();
        for h in &mut m.heads {
            clip(&mut h.name);
            total += h.channels.len();
            for c in &mut h.channels {
                fix_channel(c)?;
            }
        }
        if total > UNIVERSE_SIZE as usize {
            anyhow::bail!("mode \"{}\" has more than 512 channels", m.name);
        }
        Ok(())
    };
    for m in &mut t.modes {
        check_mode(m)?;
    }
    let p = &mut t.physical;
    for v in [&mut p.pan_range, &mut p.tilt_range, &mut p.beam_min, &mut p.beam_max] {
        *v = if v.is_finite() { v.clamp(0.0, 3600.0) } else { 0.0 };
    }
    p.emitters.retain(|e| e.is_valid() && e.attribute.is_emitter());
    p.emitters.truncate(16);
    Ok(t)
}

/// The user's fixture library folder (`<config>/fixtures`).
pub fn dir() -> Option<PathBuf> {
    crate::remote::config_dir().map(|d| d.join("fixtures"))
}

/// File name for a type in the library: its id with separators replaced.
pub fn file_name(t: &FixtureType) -> String {
    let stem: String = t.id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    format!("{stem}.{EXTENSION}")
}

/// Save a type into a library folder (atomic). Returns the file.
pub fn save(dir: &Path, t: &FixtureType) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(file_name(t));
    crate::recovery::write_atomic(&path, write(t).as_bytes())?;
    Ok(path)
}

/// Every type in a library folder, sorted by manufacturer and name; files that don't read are
/// reported as `(file name, error)`.
pub fn load_dir(dir: &Path) -> (Vec<FixtureType>, Vec<(String, String)>) {
    let mut types: Vec<FixtureType> = Vec::new();
    let mut errors = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else { return (types, errors) };
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some(EXTENSION) {
            continue;
        }
        let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let r = std::fs::metadata(&p)
            .map_err(anyhow::Error::from)
            .and_then(|m| if m.len() > super::import::MAX_FILE { Err(anyhow::anyhow!("too big")) } else { Ok(()) })
            .and_then(|_| std::fs::read_to_string(&p).map_err(anyhow::Error::from))
            .and_then(|s| read(&s));
        match r {
            Ok(t) if types.iter().any(|x| x.id == t.id) => errors.push((name, format!("same id as another file ({})", t.id))),
            Ok(t) => types.push(t),
            Err(e) => errors.push((name, format!("{e:#}"))),
        }
    }
    types.sort_by(|a, b| (a.manufacturer.to_lowercase(), a.name.to_lowercase()).cmp(&(b.manufacturer.to_lowercase(), b.name.to_lowercase())));
    (types, errors)
}

/// Remove a type's file from a library folder.
pub fn remove(dir: &Path, t: &FixtureType) -> anyhow::Result<()> {
    std::fs::remove_file(dir.join(file_name(t)))?;
    Ok(())
}

impl super::folder::Stored for FixtureType {
    const THREAD: &'static str = "openq-fixtures";
    fn load_dir(dir: &Path) -> (Vec<Self>, Vec<(String, String)>) {
        load_dir(dir)
    }
}

/// What the fixture library holds right now.
pub type Snapshot = super::folder::Snapshot<FixtureType>;

/// The fixture library ([`super::folder::Library`] of types).
pub type Library = super::folder::Library<FixtureType>;

impl Default for Library {
    fn default() -> Self {
        Self::new(dir())
    }
}

impl Library {
    /// A type by id.
    pub fn get(&self, id: &str) -> Option<FixtureType> {
        self.snapshot().items.iter().find(|t| t.id == id).cloned()
    }

    /// Save types into the folder, then reload. `done` gets the result (on the worker thread).
    pub fn save(&self, types: Vec<FixtureType>, done: impl FnOnce(anyhow::Result<()>) + Send + 'static) {
        self.job(
            move |dir| {
                for t in &types {
                    save(dir, t)?;
                }
                Ok(())
            },
            done,
        );
    }

    /// Remove a type's file, then reload.
    pub fn remove(&self, t: FixtureType, done: impl FnOnce(anyhow::Result<()>) + Send + 'static) {
        self.job(move |dir| remove(dir, &t), done);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn library_differences() {
        let a = crate::light::library::generics().into_iter().find(|t| t.modes.len() > 1).expect("a generic with modes");
        assert!(differences(&a, &a).is_empty());
        let mut b = a.clone();
        b.modes[0].channels[0].offset += 1;
        b.modes.pop();
        b.name.push('x');
        let d = differences(&b, &a);
        assert_eq!(d.len(), 3, "{d:?}");
        assert!(d[0] == "name" && d[1].ends_with(": channels") && d[2].ends_with("only in the library"), "{d:?}");
        let mut c = a.clone();
        c.modes.reverse();
        assert_eq!(differences(&c, &a), ["mode order"]);
    }
    use crate::light::library;
    use crate::light::model::{Attribute, ChannelRange, Head};

    fn bar() -> FixtureType {
        let mut t = library::generic("generic/rgb").unwrap();
        t.id = "test/bar".into();
        let m = &mut t.modes[0];
        m.channels[0].ranges.push(ChannelRange { from: 0, to: 9, name: "Open".into(), color: Some([255, 255, 255]), image: String::new() });
        m.heads = (0..4)
            .map(|i| Head { name: (i + 1).to_string(), channels: vec![ChannelDef { attribute: Attribute::Red, offset: 3 + i, ..Default::default() }] })
            .collect();
        t
    }

    #[test]
    fn file_roundtrip_and_checks() {
        let t = bar();
        let back = read(&write(&t)).unwrap();
        assert_eq!(back, t);
        assert!(read("{\"format\":\"other\",\"version\":1,\"fixture\":{}}").is_err());
        let mut bad = t.clone();
        bad.modes[0].channels[0].offset = 600;
        assert!(sanitize(bad).is_err());
        let mut bad = t.clone();
        bad.modes[0].channels[0].default = Some(f32::NAN);
        assert_eq!(sanitize(bad).unwrap().modes[0].channels[0].default, None);
        let newer = write(&t).replace("\"version\": 1", "\"version\": 99");
        assert!(read(&newer).unwrap_err().to_string().contains("newer"));
    }

    #[test]
    fn library_saves_and_loads_in_the_background() {
        let dir = std::env::temp_dir().join(format!("openq-fixlib-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let lib = Library::new(Some(dir.clone()));
        lib.wait();
        assert!(lib.snapshot().items.is_empty());
        let (tx, rx) = std::sync::mpsc::channel();
        lib.save(vec![bar()], move |r| tx.send(r.is_ok()).unwrap());
        assert!(rx.recv().unwrap());
        lib.wait();
        assert_eq!(lib.get("test/bar").unwrap().modes[0].heads.len(), 4);
        std::fs::write(dir.join("junk.oqfixture"), "not json").unwrap();
        lib.reload();
        lib.wait();
        assert_eq!(lib.snapshot().errors.len(), 1);
        let (tx, rx) = std::sync::mpsc::channel();
        lib.remove(bar(), move |r| tx.send(r.is_ok()).unwrap());
        assert!(rx.recv().unwrap());
        lib.wait();
        assert!(lib.get("test/bar").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
