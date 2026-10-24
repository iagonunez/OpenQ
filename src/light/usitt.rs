//! USITT ASCII (3.0, 1992): the plain-text show format conventional desks read and write. It
//! carries intensity only: a dimmer patch, cues with times and channel levels, groups and
//! submasters.
//!
//! OpnQ ↔ USITT:
//! * a channel is a fixture number, its level the fixture's [`Attribute::Dimmer`] (cells and
//!   other attributes are not in the format);
//! * the patch gives each channel the DMX address of its fixture's dimmer channel, counted
//!   across universes (universe 2 address 1 = dimmer 513);
//! * each sequence is a cue list: its cues are written with page = the sequence's place in the
//!   list (`Cue 5 2` = cue 5 of the second sequence), as full states (USITT cues don't track);
//! * groups are groups; submasters are playback buttons ([`super::model::LightButton`]).
//!
//! Reading ([`read`]) is lenient, like most desks: keywords in any case, compared on their first
//! ten letters; every delimiter the standard allows (space, tab, `,` `/` `;` `<` `>` `=` `@`) is
//! the same; `!` starts a comment; `$` keywords (manufacturer extensions) are skipped; levels
//! are percent, `FL` (full) or `H` + two hex digits; times are seconds or `m:ss`. [`merge`] then
//! puts a file into a design without touching what is there: new sequences, groups and buttons,
//! and generic dimmers for channels the patch doesn't have.

use super::model::{Attribute, Fixture, FixtureId, Group, LightButton, LightCue, Lighting, Look, Sequence, Value};
use std::collections::BTreeMap;

/// File extensions (`.asc` is the usual one).
pub const EXTENSIONS: [&str; 3] = ["asc", "alq", "txt"];

/// Largest file read.
pub const MAX_FILE: u64 = 32 << 20;

/// Longest line written (the standard's limit).
const LINE: usize = 80;

/// Highest channel or dimmer number read.
const MAX_NUMBER: u32 = 99_999;

/// A cue as read: full state, intensity only.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct CueRecord {
    /// Cue number as written ("1", "12.5").
    pub number: String,
    /// Page (cue list), 1 if not given.
    pub page: u32,
    /// Label.
    pub text: String,
    /// Up (fade in) time and delay, seconds.
    pub up: (f64, f64),
    /// Down (fade out) time and delay, if given.
    pub down: Option<(f64, f64)>,
    /// Channel → level (0..=1).
    pub levels: BTreeMap<u32, f32>,
    /// Follow, wait or link given (OpnQ sequences have no autofollow: left out).
    pub follow: bool,
    /// Parts given (merged into the cue, their times left out).
    pub parts: bool,
}

/// A group or submaster as read.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Record {
    /// Number.
    pub number: u32,
    /// Label.
    pub text: String,
    /// Up time (submasters), seconds.
    pub up: f64,
    /// Channel → level (0..=1).
    pub levels: BTreeMap<u32, f32>,
}

/// What a USITT ASCII file holds.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct File {
    /// Desk that wrote it (`Manufacturer` + `Console`).
    pub console: String,
    /// Channel → dimmers (absolute DMX addresses from 1), in patch order.
    pub patch: BTreeMap<u32, Vec<u32>>,
    /// Cues in file order.
    pub cues: Vec<CueRecord>,
    /// Groups.
    pub groups: Vec<Record>,
    /// Submasters.
    pub subs: Vec<Record>,
    /// Things left out or not understood.
    pub notes: Vec<String>,
}

/// A level: percent, `FL`, or `H`/`H#` + hex (`Hff` = full).
fn level(s: &str) -> Option<f32> {
    let u = s.to_ascii_uppercase();
    if u == "FL" || u == "FULL" {
        return Some(1.0);
    }
    if let Some(h) = u.strip_prefix('H') {
        let h = h.trim_start_matches('#');
        return u8::from_str_radix(h, 16).ok().map(|v| v as f32 / 255.0);
    }
    let v: f32 = u.parse().ok()?;
    v.is_finite().then(|| (v / 100.0).clamp(0.0, 1.0))
}

/// A time: seconds (`2.5`) or minutes and seconds (`1:30`).
fn time(s: &str) -> Option<f64> {
    let t = match s.split_once(':') {
        Some((m, sec)) => m.parse::<f64>().ok()? * 60.0 + sec.parse::<f64>().ok()?,
        None => s.parse::<f64>().ok()?,
    };
    (t.is_finite() && t >= 0.0).then_some(t.min(crate::limits::MAX_TIME))
}

fn number(s: &str) -> Option<u32> {
    s.parse::<u32>().ok().filter(|n| (1..=MAX_NUMBER).contains(n))
}

/// A cue number as written, if it is one ("5", "5.5"; leading zeros dropped).
fn cue_number(s: &str) -> Option<String> {
    let v: f64 = s.parse().ok()?;
    (v.is_finite() && v > 0.0 && v < 1e6 && s.chars().all(|c| c.is_ascii_digit() || c == '.')).then(|| {
        let t = s.trim_start_matches('0');
        if t.starts_with('.') { format!("0{t}") } else { t.to_string() }
    })
}

/// Which record secondary keywords belong to.
enum Current {
    None,
    Cue(usize),
    Group(usize),
    Sub(usize),
}

/// Read a USITT ASCII file.
pub fn read(text: &str) -> anyhow::Result<File> {
    let mut f = File::default();
    let mut current = Current::None;
    let mut ident = false;
    let mut unknown: BTreeMap<String, usize> = BTreeMap::new();
    let (mut bad_levels, mut extra_dimmers) = (0usize, 0usize);
    let mut manufacturer = String::new();
    for raw in text.lines() {
        let line = raw.split('!').next().unwrap_or("");
        let words: Vec<&str> = line.split(|c: char| c.is_whitespace() || ",/;<>=@".contains(c)).filter(|w| !w.is_empty()).collect();
        let Some((&kw, args)) = words.split_first() else { continue };
        // Text keeps its spacing: everything after the keyword.
        let rest = || line.trim_start()[kw.len()..].trim_start_matches(|c: char| c.is_whitespace() || ",/;<>=@".contains(c)).trim_end().to_string();
        if kw.starts_with('$') {
            continue;
        }
        let k: String = kw.to_ascii_lowercase().chars().take(10).collect();
        match k.as_str() {
            "ident" => ident = true,
            "manufactur" => manufacturer = rest(),
            "console" => f.console = if manufacturer.is_empty() { rest() } else { format!("{manufacturer} {}", rest()) },
            "clear" | "set" => {}
            "enddata" => break,
            "patch" => {
                current = Current::None;
                // Page, then channel, dimmer, level triples.
                for t in args.get(1..).unwrap_or(&[]).chunks(3) {
                    let (Some(ch), Some(d)) = (t.first().and_then(|c| number(c)), t.get(1).and_then(|d| number(d))) else {
                        bad_levels += 1;
                        continue;
                    };
                    let list = f.patch.entry(ch).or_default();
                    if !list.contains(&d) {
                        list.push(d);
                        extra_dimmers += (list.len() == 2) as usize;
                    }
                }
            }
            "cue" => {
                let Some(n) = args.first().and_then(|n| cue_number(n)) else {
                    f.notes.push(format!("cue \"{}\" is not a cue number, left out", args.first().unwrap_or(&"")));
                    current = Current::None;
                    continue;
                };
                let page = args.get(1).and_then(|p| number(p)).unwrap_or(1);
                f.cues.push(CueRecord { number: n, page, up: (0.0, 0.0), ..Default::default() });
                current = Current::Cue(f.cues.len() - 1);
            }
            "group" | "sub" => {
                let Some(n) = args.first().and_then(|n| number(n)) else {
                    current = Current::None;
                    continue;
                };
                let r = Record { number: n, ..Default::default() };
                if k == "group" {
                    f.groups.push(r);
                    current = Current::Group(f.groups.len() - 1);
                } else {
                    f.subs.push(r);
                    current = Current::Sub(f.subs.len() - 1);
                }
            }
            "text" => match current {
                Current::Cue(i) => f.cues[i].text = rest(),
                Current::Group(i) => f.groups[i].text = rest(),
                Current::Sub(i) => f.subs[i].text = rest(),
                Current::None => {}
            },
            "up" | "down" => {
                let t = (args.first().and_then(|t| time(t)).unwrap_or(0.0), args.get(1).and_then(|t| time(t)).unwrap_or(0.0));
                match current {
                    Current::Cue(i) if k == "up" => f.cues[i].up = t,
                    Current::Cue(i) => f.cues[i].down = Some(t),
                    Current::Sub(i) if k == "up" => f.subs[i].up = t.0,
                    _ => {}
                }
            }
            "wait" | "follow" | "link" => {
                if let Current::Cue(i) = current {
                    f.cues[i].follow = true;
                }
            }
            "part" => {
                if let Current::Cue(i) = current {
                    f.cues[i].parts = true;
                }
            }
            "chan" => {
                let levels = match current {
                    Current::Cue(i) => &mut f.cues[i].levels,
                    Current::Group(i) => &mut f.groups[i].levels,
                    Current::Sub(i) => &mut f.subs[i].levels,
                    Current::None => continue,
                };
                for p in args.chunks(2) {
                    match (number(p[0]), p.get(1).and_then(|l| level(l))) {
                        (Some(ch), Some(v)) => {
                            levels.insert(ch, v);
                        }
                        _ => bad_levels += 1,
                    }
                }
            }
            _ => *unknown.entry(kw.to_string()).or_default() += 1,
        }
    }
    if !ident && f.cues.is_empty() && f.patch.is_empty() && f.groups.is_empty() && f.subs.is_empty() {
        anyhow::bail!("not a USITT ASCII file (no Ident, patch, cues or groups)");
    }
    if extra_dimmers > 0 {
        f.notes.push(format!("{extra_dimmers} channels have more than one dimmer: only the first is patched"));
    }
    if bad_levels > 0 {
        f.notes.push(format!("{bad_levels} channel or patch entries could not be read"));
    }
    for (kw, n) in unknown {
        f.notes.push(format!("keyword \"{kw}\" not understood ({n}×)"));
    }
    let follows = f.cues.iter().filter(|c| c.follow).count();
    if follows > 0 {
        f.notes.push(format!("{follows} cues have follow, wait or link times: left out (GO them from the cue list)"));
    }
    let parts = f.cues.iter().filter(|c| c.parts).count();
    if parts > 0 {
        f.notes.push(format!("{parts} cues have parts: their levels are in the cue, their times left out"));
    }
    Ok(f)
}

/// Lowest unused number at or above 1 in `taken`.
fn free(taken: impl Iterator<Item = u32>, want: u32) -> u32 {
    let taken: std::collections::HashSet<u32> = taken.collect();
    if want > 0 && !taken.contains(&want) {
        return want;
    }
    (1..).find(|n| !taken.contains(n)).expect("a free number")
}

/// Put a file into a design: generic dimmers for channels without a fixture (patched where the
/// file says, else unpatched), one new sequence per cue page (named `name`, plus the page when
/// there are several), new groups and playback buttons for submasters. Nothing already in the
/// design changes. Returns notes about what was left out.
pub fn merge(l: &mut Lighting, f: &File, name: &str) -> Vec<String> {
    let mut notes = f.notes.clone();
    let dimmer = super::library::generics().into_iter().find(|t| t.id == "generic/dimmer").expect("built-in dimmer");
    let mode = dimmer.modes[0].name.clone();
    // Fixtures for every channel used.
    let mut used: std::collections::BTreeSet<u32> = f.patch.keys().copied().collect();
    for levels in f.cues.iter().map(|c| &c.levels).chain(f.groups.iter().map(|g| &g.levels)).chain(f.subs.iter().map(|s| &s.levels)) {
        used.extend(levels.keys());
    }
    let max_id = super::model::MAX_FIXTURE_ID;
    let (mut unpatched, mut moved) = (0usize, 0usize);
    for ch in used {
        if ch > max_id || l.fixture(ch).is_some() {
            if f.patch.contains_key(&ch) && l.fixture(ch).is_some() {
                moved += 1;
            }
            continue;
        }
        let (universe, address) = match f.patch.get(&ch).and_then(|d| d.first()) {
            Some(&d) => (((d - 1) / 512 + 1).min(super::model::MAX_UNIVERSE as u32) as u16, ((d - 1) % 512 + 1) as u16),
            None => {
                unpatched += 1;
                (1, 0)
            }
        };
        l.add_type(&dimmer);
        l.fixtures.push(Fixture { id: ch, fixture_type: dimmer.id.clone(), mode: mode.clone(), universe, address, ..Default::default() });
    }
    if moved > 0 {
        notes.push(format!("{moved} channels are fixtures in this show already: their patch was kept"));
    }
    if unpatched > 0 {
        notes.push(format!("{unpatched} channels used in cues are not in the patch: added unpatched"));
    }
    let level = |v: f32| BTreeMap::from([(Attribute::Dimmer, Value::Level(v))]);
    // Cues: one sequence per page, full states turned into tracking changes.
    let mut pages: Vec<u32> = f.cues.iter().map(|c| c.page).collect();
    pages.sort_unstable();
    pages.dedup();
    let mut downs = 0usize;
    for page in &pages {
        let id = free(l.sequences.iter().map(|s| s.id), 1);
        let label = if pages.len() > 1 { format!("{name} {page}") } else { name.to_string() };
        let mut cues = Vec::new();
        let mut state: BTreeMap<u32, f32> = BTreeMap::new();
        for c in f.cues.iter().filter(|c| c.page == *page) {
            let mut values = Look::new();
            for ch in state.keys().chain(c.levels.keys()).copied().collect::<std::collections::BTreeSet<_>>() {
                let (was, now) = (state.get(&ch).copied().unwrap_or(0.0), c.levels.get(&ch).copied().unwrap_or(0.0));
                if (was - now).abs() > 1e-6 || (cues.is_empty() && now > 0.0) {
                    values.insert(ch as FixtureId, level(now));
                }
            }
            state = c.levels.clone();
            let down = c.down.filter(|d| d.0 != c.up.0).map(|d| d.0);
            downs += c.down.is_some_and(|d| d.1 > 0.0) as usize;
            cues.push(LightCue { number: c.number.clone(), name: c.text.clone(), fade: c.up.0, down_fade: down, delay: c.up.1, values: values.into(), ..Default::default() });
        }
        l.sequences.push(Sequence { id, name: label, cues: cues.into(), ..Default::default() });
    }
    if downs > 0 {
        notes.push(format!("{downs} cues have a down delay: left out"));
    }
    for g in &f.groups {
        let id = free(l.groups.iter().map(|g| g.id), g.number);
        let fixtures = g.levels.iter().filter(|(_, v)| **v > 0.0).map(|(c, _)| *c as FixtureId).collect();
        l.groups.push(Group { id, name: if g.text.is_empty() { format!("{name} {}", g.number) } else { g.text.clone() }, fixtures });
    }
    for s in &f.subs {
        let id = free(l.buttons.iter().map(|b| b.id), s.number);
        let values: Look = s.levels.iter().map(|(c, v)| (*c as FixtureId, level(*v))).collect();
        let label = if s.text.is_empty() { format!("Sub {}", s.number) } else { s.text.clone() };
        l.buttons.push(LightButton { id, name: label, values: values.into(), fade: s.up, ..Default::default() });
    }
    notes
}

/// Lines of `head` followed by `items`, wrapped at the standard's line length (each line
/// starts with `head` again).
fn wrapped(out: &mut String, head: &str, items: impl IntoIterator<Item = String>) {
    let mut line = head.to_string();
    let mut any = false;
    for it in items {
        if any && line.len() + 1 + it.len() > LINE {
            out.push_str(&line);
            out.push('\n');
            line = head.to_string();
        }
        line.push(' ');
        line.push_str(&it);
        any = true;
    }
    if any {
        out.push_str(&line);
        out.push('\n');
    }
}

fn level_text(v: f32) -> String {
    let p = (v.clamp(0.0, 1.0) * 100.0).round() as u32;
    if p >= 100 { "FL".into() } else { p.to_string() }
}

fn time_text(t: f64) -> String {
    let t = (t.max(0.0) * 100.0).round() / 100.0;
    format!("{t}")
}

/// `Text` line (ASCII, cut to the line length); a `$$UText` line too when the label has other
/// characters (decimal character references, as Eos reads them).
fn text_lines(out: &mut String, s: &str) {
    let s = s.trim();
    if s.is_empty() {
        return;
    }
    let ascii: String = s.chars().map(|c| if c.is_ascii() && !c.is_ascii_control() && c != '!' { c } else { '?' }).take(LINE - 5).collect();
    out.push_str(&format!("Text {ascii}\n"));
    if !s.is_ascii() {
        let refs: String = s.chars().map(|c| if c.is_ascii() && !c.is_ascii_control() && c != '!' { c.to_string() } else { format!("&#{};", c as u32) }).collect();
        out.push_str(&format!("$$UText {}\n", refs.chars().take(LINE * 4).collect::<String>()));
    }
}

/// Intensity of a fixture in a look (presets resolved), if set.
fn intensity(l: &Lighting, look: &Look, f: FixtureId) -> Option<f32> {
    look.get(&f)?.get(&Attribute::Dimmer).and_then(|v| l.resolve(f, Attribute::Dimmer, *v))
}

/// The design as a USITT ASCII file: patch, groups, playback buttons as submasters, and every
/// sequence as a cue page. `scope` (the show's name) goes into a comment. Returns the text and
/// notes about what is not in the format.
pub fn write(l: &Lighting, scope: &str) -> (String, Vec<String>) {
    let mut out = String::new();
    let mut notes = Vec::new();
    out.push_str(&format!("! {} — OpnQ {}\n", scope.chars().filter(|c| c.is_ascii() && *c != '!').collect::<String>(), env!("CARGO_PKG_VERSION")));
    out.push_str("Ident 3:0\nManufacturer OpnQ\nConsole OpnQ\nClear All\n");
    // Patch: the fixture's dimmer channel (fixtures without one have only a virtual dimmer).
    let mut patch = Vec::new();
    let mut no_dimmer = 0usize;
    for fx in l.fixtures.iter() {
        let Some(m) = l.mode_of(fx) else { continue };
        match m.channel(Attribute::Dimmer) {
            Some(c) => {
                let at = fx.address_of(c.dmx_break);
                if at.address > 0 && at.universe > 0 {
                    patch.push((fx.id, (at.universe as u32 - 1) * 512 + at.address as u32 + c.offset as u32));
                }
            }
            None => no_dimmer += 1,
        }
    }
    if no_dimmer > 0 {
        notes.push(format!("{no_dimmer} fixtures have no dimmer channel: in the cues, not in the patch"));
    }
    if let Some(ch) = l.fixtures.iter().map(|f| f.id).max() {
        out.push_str(&format!("Set Channels {ch}\n"));
    }
    if let Some(d) = patch.iter().map(|p| p.1).max() {
        out.push_str(&format!("Set Dimmers {d}\n"));
    }
    wrapped(&mut out, "Patch 1", patch.iter().map(|(c, d)| format!("{c}<{d}@FL")));
    let fixture_ids = |look: &Look| look.keys().copied().filter(|f| super::model::split_id(*f).1.is_none()).collect::<Vec<_>>();
    for g in &l.groups {
        out.push_str(&format!("Group {}\n", g.id));
        text_lines(&mut out, &g.name);
        wrapped(&mut out, "Chan", g.fixtures.iter().filter(|f| super::model::split_id(**f).1.is_none()).map(|f| format!("{f}@FL")));
    }
    for b in &l.buttons {
        let look = b.look(l);
        out.push_str(&format!("Sub {}\n", b.id));
        text_lines(&mut out, &b.name);
        if b.fade > 0.0 {
            out.push_str(&format!("Up {}\nDown {}\n", time_text(b.fade), time_text(b.fade)));
        }
        wrapped(&mut out, "Chan", fixture_ids(&look).into_iter().filter_map(|f| Some(format!("{f}@{}", level_text(intensity(l, &look, f)?)))));
    }
    let mut renumbered = 0usize;
    let mut cells = false;
    for (page, s) in l.sequences.iter().enumerate() {
        let mut last = 0.0f64;
        for (i, c) in s.cues.iter().enumerate() {
            let number = match cue_number(c.number.trim()).filter(|n| n.parse::<f64>().is_ok_and(|v| v > last)) {
                Some(n) => n,
                None => {
                    renumbered += 1;
                    format!("{}", (last.floor() + 1.0) as u64)
                }
            };
            last = number.parse().unwrap_or(last);
            out.push_str(&format!("Cue {number} {}\n", page + 1));
            text_lines(&mut out, &c.name);
            out.push_str(&format!("Up {}{}\n", time_text(c.fade), if c.delay > 0.0 { format!(" {}", time_text(c.delay)) } else { String::new() }));
            out.push_str(&format!("Down {}\n", time_text(c.down_fade.unwrap_or(c.fade))));
            let state = s.tracked(i);
            cells |= state.keys().any(|f| super::model::split_id(*f).1.is_some());
            let levels = fixture_ids(&state).into_iter().filter_map(|f| intensity(l, &state, f).filter(|v| *v > 0.0).map(|v| format!("{f}@{}", level_text(v))));
            wrapped(&mut out, "Chan", levels);
        }
    }
    if renumbered > 0 {
        notes.push(format!("{renumbered} cues had numbers USITT can't hold (not rising decimals): renumbered"));
    }
    if cells {
        notes.push("cell intensities are not in the file (only whole fixtures)".into());
    }
    out.push_str("EndData\n");
    (out, notes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::light::library;

    const ETC: &str = "Ident 3:0\nManufacturer ETC\nConsole Eos\n$$Format 3.20\nClear All\nSet Channels 100\n\
        Patch 1 1<1@Hff 2<2@Hff 3<513@FL\nPatch 1 3<514@FL 4<5@FL\n! a comment\n\
        Group 1\nText Front wash\nChan 1@FL 2@FL\n\
        Sub 2\nText Fog\nUp 3\nChan 4@80\n\
        Cue 1 1\nText Preset ! ignored\nUp 5 1\nDown 3\nChan 1@50 2@H80\n\
        Cue 1.5,1\nUp 1:30\nchan 2/FL, 3/25\nFollow 2\n\
        CUE 2 1\nUP 2\nCHAN 1 75\n\
        Cue 7 2\nChan 4@FL\nPart 1\nFrobnicate 3\nEndData\nCue 99\n";

    #[test]
    fn reads_a_desk_file() {
        let f = read(ETC).unwrap();
        assert_eq!(f.console, "ETC Eos");
        assert_eq!(f.patch[&3], vec![513, 514]);
        assert_eq!(f.patch[&4], vec![5]);
        assert_eq!(f.cues.len(), 4, "EndData stops reading");
        let c = &f.cues[0];
        assert_eq!((c.number.as_str(), c.page, c.text.as_str(), c.up, c.down), ("1", 1, "Preset", (5.0, 1.0), Some((3.0, 0.0))));
        assert_eq!(c.levels[&1], 0.5);
        assert!((c.levels[&2] - 128.0 / 255.0).abs() < 1e-6);
        assert_eq!(f.cues[1].number, "1.5");
        assert_eq!(f.cues[1].up.0, 90.0);
        assert!(f.cues[1].follow);
        assert_eq!(f.cues[1].levels[&3], 0.25);
        assert_eq!(f.cues[2].levels[&1], 0.75, "keywords in any case, space-separated pairs");
        assert_eq!(f.cues[3].page, 2);
        assert_eq!(f.groups[0].text, "Front wash");
        assert_eq!((f.subs[0].number, f.subs[0].up, f.subs[0].levels[&4]), (2, 3.0, 0.8));
        let notes = f.notes.join("\n");
        assert!(notes.contains("more than one dimmer") && notes.contains("Frobnicate") && notes.contains("follow"), "{notes}");
        assert!(read("hello\nworld\n").is_err());
        assert!(read("").is_err());
    }

    #[test]
    fn merge_adds_and_keeps() {
        let f = read(ETC).unwrap();
        let mut l = library::bench_rig(2, 1, 1);
        let before = l.fixtures.len();
        let seqs = l.sequences.len();
        let notes = merge(&mut l, &f, "Imported");
        // Channels 1 and 2 are fixtures already; 3 and 4 become dimmers.
        assert_eq!(l.fixtures.len(), before + 2);
        let d3 = l.fixture(3).unwrap();
        assert_eq!((d3.universe, d3.address, d3.fixture_type.as_str()), (2, 1, "generic/dimmer"));
        assert_eq!(l.sequences.len(), seqs + 2, "two pages");
        let s = l.sequences.iter().find(|s| s.name == "Imported 1").unwrap();
        assert_eq!(s.cues.len(), 3);
        // Tracking: cue 2 sets channel 1 to 75 and takes 2 and 3 out.
        let c2 = &s.cues[2];
        assert_eq!(c2.values[&1][&Attribute::Dimmer], Value::Level(0.75));
        assert_eq!(c2.values[&2][&Attribute::Dimmer], Value::Level(0.0));
        assert_eq!(c2.values[&3][&Attribute::Dimmer], Value::Level(0.0));
        assert_eq!(s.cues[0].fade, 5.0);
        assert_eq!(s.cues[0].delay, 1.0);
        assert_eq!(s.cues[0].down_fade, Some(3.0));
        // Played back, the tracked state is the file's full state.
        let state = s.tracked(1);
        assert_eq!(state[&1][&Attribute::Dimmer], Value::Level(0.0), "not in cue 1.5: out");
        assert_eq!(state[&2][&Attribute::Dimmer], Value::Level(1.0));
        assert_eq!(state[&3][&Attribute::Dimmer], Value::Level(0.25));
        assert!(l.groups.iter().any(|g| g.name == "Front wash" && g.fixtures == [1, 2]));
        let fog = l.buttons.iter().find(|b| b.name == "Fog").unwrap();
        assert_eq!(fog.fade, 3.0);
        assert!(notes.iter().any(|n| n.contains("patch was kept")), "{notes:?}");
    }

    #[test]
    fn written_files_read_back() {
        let mut l = library::bench_rig(30, 1, 1);
        let ids: Vec<u32> = l.fixtures.iter().map(|f| f.id).collect();
        let mut look = Look::new();
        for (i, f) in ids.iter().enumerate() {
            look.insert(*f, BTreeMap::from([(Attribute::Dimmer, Value::Level(i as f32 / 30.0)), (Attribute::Pan, Value::Level(0.5))]));
        }
        let mut cue2 = Look::new();
        cue2.insert(ids[0], BTreeMap::from([(Attribute::Dimmer, Value::Level(1.0))]));
        l.sequences = vec![Sequence {
            id: 4,
            name: "Main".into(),
            cues: vec![
                LightCue { number: "1".into(), name: "Préset ä".into(), fade: 3.0, delay: 0.5, values: look.into(), ..Default::default() },
                LightCue { number: "x".into(), fade: 1.25, down_fade: Some(4.0), values: cue2.into(), ..Default::default() },
            ]
            .into(),
            ..Default::default()
        }];
        l.groups.push(Group { id: 3, name: "All".into(), fixtures: ids.clone() });
        let (text, notes) = write(&l, "Show");
        assert!(text.lines().all(|x| x.len() <= LINE || x.starts_with("$$")), "{text}");
        assert!(text.contains("Text Pr?set ?") && text.contains("$$UText Pr&#233;set &#228;"), "{text}");
        assert!(notes.iter().any(|n| n.contains("renumbered")));
        let f = read(&text).unwrap();
        assert!(f.notes.is_empty(), "{:?}", f.notes);
        assert_eq!(f.patch.len(), l.fixtures.iter().filter(|x| x.address > 0).count());
        assert_eq!(f.cues.len(), 2);
        assert_eq!(f.cues[1].number, "2");
        assert_eq!((f.cues[0].up, f.cues[1].down), ((3.0, 0.5), Some((4.0, 0.0))));
        // Full states: cue 2 tracks the rest of cue 1.
        assert_eq!(f.cues[1].levels[&ids[0]], 1.0);
        assert_eq!(f.cues[1].levels[&ids[29]], f.cues[0].levels[&ids[29]]);
        assert!((f.cues[0].levels[&ids[15]] - 0.5).abs() < 0.006);
        assert_eq!(f.groups[0].levels.len(), 30);
        // Into an empty design: the same fixture numbers, patched at the same dimmers.
        let mut fresh = Lighting::default();
        merge(&mut fresh, &f, "Show");
        let d = fresh.fixture(ids[1]).unwrap();
        let src = l.fixture(ids[1]).unwrap();
        let off = l.mode_of(src).unwrap().channel(Attribute::Dimmer).unwrap().offset;
        assert_eq!((d.universe, d.address), (src.universe, src.address + off));
    }
}
