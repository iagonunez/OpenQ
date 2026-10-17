//! The lighting command line: desk-style syntax parsed into [`Cmd`]s.
//!
//! ```text
//! 1 thru 10 at 50          select 1..10, intensity 50 %
//! 101.1 thru 101.12 at 80  cells 1..12 of multi-cell fixture 101
//! group 2 + 7 - 3 @ full   group 2 plus 7 minus 3, full
//! 101 > 104 preset 3       ">" is thru; apply preset 3
//! all at out               "out" = 0
//! effect 2                 run effect 2 on the selection
//! gobo star | wheel 3      pick a named range of a channel by name (prefix) or number;
//! shutter "strobe fast"    words: gobo gobo2 wheel wheel2 macro prism prism2 shutter iris frost frost2 control control2 control3
//! record cue 5 / 2         record the programmer into cue 5 of sequence 2 ("/ 2" optional)
//! record group 4           store the selection as group 4 (also: record preset 6)
//! update | clear | blind | highlight
//! go 2 | go cue 5 / 2 | release 2 | release      sequence playback
//! delete cue 5 / 2 | delete group 4 | delete preset 6
//! ```
//!
//! Keywords can be abbreviated to any unique prefix of at least two letters (`th`, `gr`,
//! `rec`). Parsing is pure; the UI applies the commands.

use super::model::{Attribute, FixtureId, GroupId, PresetId, SequenceId};

/// How a selection term changes the selection.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SelOp {
    /// Start a new selection.
    Replace,
    /// Add to it.
    Add,
    /// Take out of it.
    Remove,
}

/// What a selection term names.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SelSet {
    /// One fixture or cell.
    Fixture(FixtureId),
    /// Fixtures (or cells) numbered from..=to (patched ones only).
    Range(FixtureId, FixtureId),
    /// A group.
    Group(GroupId),
    /// Every fixture.
    All,
}

/// What record and delete act on.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Target {
    /// A lighting cue by number, in a sequence (None = the current one).
    Cue(f64, Option<SequenceId>),
    /// A group.
    Group(GroupId),
    /// A preset.
    Preset(PresetId),
}

/// One parsed command.
#[derive(Clone, PartialEq, Debug)]
pub enum Cmd {
    /// Change the selection.
    Select(SelOp, SelSet),
    /// Intensity of the selection, 0..=1.
    At(f32),
    /// Apply a preset to the selection.
    Preset(PresetId),
    /// Run an effect on the selection.
    Effect(u32),
    /// Set an attribute of the selection to one of its named ranges (name, prefix or number).
    PickRange(Attribute, String),
    /// Empty the programmer.
    Clear,
    /// Record the programmer / selection.
    Record(Target),
    /// Merge the programmer into the selected cue.
    Update,
    /// Delete something.
    Delete(Target),
    /// GO the next cue of a sequence (None = the current one), or a given cue.
    Go(Option<SequenceId>, Option<f64>),
    /// Release a sequence (None = all).
    Release(Option<SequenceId>),
    /// Toggle blind.
    Blind,
    /// Toggle highlight.
    Highlight,
}

const KEYWORDS: [&str; 17] =
    ["thru", "at", "full", "out", "group", "all", "preset", "effect", "clear", "record", "update", "delete", "cue", "go", "release", "blind", "highlight"];

/// Range words: the attribute each picks a named range of.
const RANGE_WORDS: [(&str, Attribute); 17] = [
    ("gobo", Attribute::Gobo),
    ("gobo2", Attribute::Gobo2),
    ("wheel2", Attribute::ColorWheel2),
    ("prism2", Attribute::Prism2),
    ("frost2", Attribute::Frost2),
    ("control2", Attribute::Control2),
    ("control3", Attribute::Control3),
    ("wheel", Attribute::ColorWheel),
    ("colour", Attribute::ColorWheel),
    ("color", Attribute::ColorWheel),
    ("macro", Attribute::ColorMacro),
    ("prism", Attribute::Prism),
    ("shutter", Attribute::Shutter),
    ("strobe", Attribute::Shutter),
    ("iris", Attribute::Iris),
    ("frost", Attribute::Frost),
    ("control", Attribute::Control),
];

/// Range words ending in a digit (kept whole: "gobo2 3" is gobo 2, slot 3).
const NUMBERED_WORDS: [&str; 6] = ["gobo2", "wheel2", "prism2", "frost2", "control2", "control3"];

/// Split into tokens: numbers, words, `"quoted text"` (kept with its quote mark), and the
/// symbols `+ - > @ /`.
fn tokens(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let flush = |cur: &mut String, out: &mut Vec<String>| {
        if !cur.is_empty() {
            out.push(std::mem::take(cur));
        }
    };
    let mut quoted: Option<String> = None;
    for ch in s.chars() {
        if let Some(q) = &mut quoted {
            if ch == '"' {
                out.push(quoted.take().expect("quoted"));
            } else {
                q.push(ch);
            }
            continue;
        }
        match ch {
            '"' => {
                flush(&mut cur, &mut out);
                quoted = Some("\"".into());
            }
            '+' | '-' | '>' | '@' | '/' => {
                flush(&mut cur, &mut out);
                out.push(ch.to_string());
            }
            c if c.is_whitespace() => flush(&mut cur, &mut out),
            c => {
                // A digit right after letters starts a new token ("g2" = "g 2"), and the reverse
                // ("gobo2" is a word of its own).
                if cur.chars().last().is_some_and(|l| l.is_ascii_alphabetic() != c.is_ascii_alphabetic() && c != '.' && l != '.') && !NUMBERED_WORDS.contains(&format!("{cur}{c}").as_str())
                {
                    flush(&mut cur, &mut out);
                }
                cur.push(c.to_ascii_lowercase());
            }
        }
    }
    flush(&mut cur, &mut out);
    if let Some(q) = quoted {
        out.push(q); // unclosed quote: up to the end
    }
    out
}

/// A word as a keyword (exact, or a unique prefix of two letters or more).
fn keyword(w: &str) -> Option<&'static str> {
    if let Some(k) = KEYWORDS.iter().find(|k| **k == w) {
        return Some(k);
    }
    match w {
        "t" => return Some("thru"),
        "ff" => return Some("full"),
        "c" => return Some("clear"),
        "g" => return Some("group"),
        _ => {}
    }
    if w.len() < 2 {
        return None;
    }
    let mut hits = KEYWORDS.iter().filter(|k| k.starts_with(w));
    match (hits.next(), hits.next()) {
        (Some(k), None) => Some(k),
        _ => None,
    }
}

struct Parser {
    toks: Vec<String>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&str> {
        self.toks.get(self.pos).map(|s| s.as_str())
    }

    fn next(&mut self) -> Option<String> {
        let t = self.toks.get(self.pos).cloned();
        self.pos += 1;
        t
    }

    fn kw(&self) -> Option<&'static str> {
        self.peek().and_then(keyword)
    }

    fn eat(&mut self, k: &str) -> bool {
        if self.kw() == Some(k) || self.peek() == Some(k) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn number(&mut self) -> Result<f64, String> {
        match self.next() {
            Some(t) => t.parse::<f64>().map_err(|_| format!("expected a number, got \"{t}\"")),
            None => Err("expected a number".into()),
        }
    }

    fn int(&mut self) -> Result<u32, String> {
        let n = self.number()?;
        if n < 0.0 || n.fract() != 0.0 || n > u32::MAX as f64 {
            return Err(format!("{n} is not a whole number"));
        }
        Ok(n as u32)
    }

    fn is_number(&self) -> bool {
        self.peek().is_some_and(|t| t.parse::<f64>().is_ok())
    }

    /// A fixture or cell number: `101`, `101.3`.
    fn fixture(&mut self) -> Result<FixtureId, String> {
        match self.next() {
            Some(t) => super::model::parse_fixture(&t).ok_or_else(|| format!("\"{t}\" is not a fixture number")),
            None => Err("expected a fixture number".into()),
        }
    }

    /// `N`, `N thru M`, `group N`, `all`.
    fn set(&mut self) -> Result<SelSet, String> {
        if self.eat("group") {
            return Ok(SelSet::Group(self.int()?));
        }
        if self.eat("all") {
            return Ok(SelSet::All);
        }
        let a = self.fixture()?;
        if self.eat("thru") || self.eat(">") {
            let b = self.fixture()?;
            return Ok(SelSet::Range(a.min(b), a.max(b)));
        }
        Ok(SelSet::Fixture(a))
    }

    /// `cue N [/ S]`, `group N`, `preset N`; a bare number is a cue.
    fn target(&mut self) -> Result<Target, String> {
        if self.eat("group") {
            return Ok(Target::Group(self.int()?));
        }
        if self.eat("preset") {
            return Ok(Target::Preset(self.int()?));
        }
        self.eat("cue");
        let n = self.number()?;
        let seq = if self.eat("/") { Some(self.int()?) } else { None };
        Ok(Target::Cue(n, seq))
    }

    fn level(&mut self) -> Result<f32, String> {
        if self.eat("full") {
            return Ok(1.0);
        }
        if self.eat("out") {
            return Ok(0.0);
        }
        let v = self.number()?;
        if !(0.0..=100.0).contains(&v) {
            return Err(format!("{v} is not a level from 0 to 100"));
        }
        Ok(v as f32 / 100.0)
    }
}

/// Parse a command line.
pub fn parse(line: &str) -> Result<Vec<Cmd>, String> {
    let mut p = Parser { toks: tokens(line), pos: 0 };
    let mut out = Vec::new();
    while let Some(tok) = p.peek().map(str::to_string) {
        let kw = p.kw();
        if let Some((_, attr)) = RANGE_WORDS.iter().find(|(w, _)| *w == tok) {
            p.pos += 1;
            let name = match p.next() {
                Some(n) if n.starts_with('"') => n[1..].to_string(),
                Some(n) => n,
                None => return Err(format!("{tok}: which range?")),
            };
            if name.trim().is_empty() {
                return Err(format!("{tok}: which range?"));
            }
            out.push(Cmd::PickRange(*attr, name));
            continue;
        }
        match kw {
            _ if tok == "+" || tok == "-" => {
                p.pos += 1;
                let op = if tok == "+" { SelOp::Add } else { SelOp::Remove };
                out.push(Cmd::Select(op, p.set()?));
            }
            _ if tok == "@" => {
                p.pos += 1;
                out.push(at(&mut p)?);
            }
            Some("at") => {
                p.pos += 1;
                out.push(at(&mut p)?);
            }
            Some("group") | Some("all") => out.push(Cmd::Select(SelOp::Replace, p.set()?)),
            _ if p.is_number() => out.push(Cmd::Select(SelOp::Replace, p.set()?)),
            Some("full") | Some("out") => out.push(Cmd::At(p.level()?)),
            Some("preset") => {
                p.pos += 1;
                out.push(Cmd::Preset(p.int()?));
            }
            Some("effect") => {
                p.pos += 1;
                out.push(Cmd::Effect(p.int()?));
            }
            Some("clear") => {
                p.pos += 1;
                out.push(Cmd::Clear);
            }
            Some("update") => {
                p.pos += 1;
                out.push(Cmd::Update);
            }
            Some("blind") => {
                p.pos += 1;
                out.push(Cmd::Blind);
            }
            Some("highlight") => {
                p.pos += 1;
                out.push(Cmd::Highlight);
            }
            Some("record") => {
                p.pos += 1;
                out.push(Cmd::Record(p.target()?));
            }
            Some("delete") => {
                p.pos += 1;
                out.push(Cmd::Delete(p.target()?));
            }
            Some("go") => {
                p.pos += 1;
                if p.eat("cue") || p.is_number() && p.toks.get(p.pos + 1).is_some_and(|t| t == "/") {
                    let Target::Cue(n, seq) = p.target()? else { return Err("go cue: expected a cue number".into()) };
                    out.push(Cmd::Go(seq, Some(n)));
                } else {
                    let seq = if p.is_number() { Some(p.int()?) } else { None };
                    out.push(Cmd::Go(seq, None));
                }
            }
            Some("release") => {
                p.pos += 1;
                let seq = if p.is_number() { Some(p.int()?) } else { None };
                out.push(Cmd::Release(seq));
            }
            _ => return Err(format!("don't know \"{tok}\"")),
        }
    }
    Ok(out)
}

/// After `at`: a level or `preset N`.
fn at(p: &mut Parser) -> Result<Cmd, String> {
    if p.eat("preset") {
        return Ok(Cmd::Preset(p.int()?));
    }
    Ok(Cmd::At(p.level()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use Cmd::*;
    use SelOp::*;

    #[test]
    fn go_cue_needs_a_cue() {
        assert!(parse("go cue preset 0").is_err());
        assert!(parse("go cue group 2").is_err());
    }
    use SelSet::*;

    #[test]
    fn selections_and_levels() {
        assert_eq!(parse("1 thru 10 at 50").unwrap(), [Select(Replace, Range(1, 10)), At(0.5)]);
        assert_eq!(parse("group 2 + 7 - 3 @ full").unwrap(), [Select(Replace, Group(2)), Select(Add, Fixture(7)), Select(Remove, Fixture(3)), At(1.0)]);
        assert_eq!(parse("104>101 ff").unwrap(), [Select(Replace, Range(101, 104)), At(1.0)]);
        assert_eq!(parse("g2@0").unwrap(), [Select(Replace, Group(2)), At(0.0)]);
        assert_eq!(parse("all at out").unwrap(), [Select(Replace, All), At(0.0)]);
        assert_eq!(parse("5 th 8 at preset 3").unwrap(), [Select(Replace, Range(5, 8)), Preset(3)]);
        assert_eq!(parse("AT 33.5").unwrap(), [At(0.335)]);
        use crate::light::model::cell_id;
        assert_eq!(parse("101.1 thru 101.12 at 80").unwrap(), [Select(Replace, Range(cell_id(101, 1), cell_id(101, 12))), At(0.8)]);
    }

    #[test]
    fn ranges() {
        let r = |a, n: &str| PickRange(a, n.to_string());
        assert_eq!(parse("1 gobo star").unwrap(), [Select(Replace, Fixture(1)), r(Attribute::Gobo, "star")]);
        assert_eq!(parse("gobo2 3").unwrap(), [r(Attribute::Gobo2, "3")]);
        assert_eq!(parse("gobo 2").unwrap(), [r(Attribute::Gobo, "2")]);
        assert_eq!(parse("gobo3").unwrap(), [r(Attribute::Gobo, "3")], "only \"gobo2\" is a word");
        assert_eq!(parse("wheel2 4").unwrap(), [r(Attribute::ColorWheel2, "4")]);
        assert_eq!(parse("control3 \"reset\"").unwrap(), [r(Attribute::Control3, "reset")]);
        assert_eq!(parse("shutter \"Strobe fast\" at full").unwrap(), [r(Attribute::Shutter, "Strobe fast"), At(1.0)]);
        assert_eq!(parse("wheel \"deep red").unwrap(), [r(Attribute::ColorWheel, "deep red")]);
    }

    #[test]
    fn record_delete_playback() {
        assert_eq!(parse("record cue 5 / 2").unwrap(), [Record(Target::Cue(5.0, Some(2)))]);
        assert_eq!(parse("rec 2.5").unwrap(), [Record(Target::Cue(2.5, None))]);
        assert_eq!(parse("record group 4").unwrap(), [Record(Target::Group(4))]);
        assert_eq!(parse("del preset 6").unwrap(), [Delete(Target::Preset(6))]);
        assert_eq!(parse("go").unwrap(), [Go(None, None)]);
        assert_eq!(parse("go 2").unwrap(), [Go(Some(2), None)]);
        assert_eq!(parse("go cue 5 / 2").unwrap(), [Go(Some(2), Some(5.0))]);
        assert_eq!(parse("go 5/2").unwrap(), [Go(Some(2), Some(5.0))]);
        assert_eq!(parse("rel").unwrap(), [Release(None)]);
        assert_eq!(parse("clear").unwrap(), [Clear]);
        assert_eq!(parse("c").unwrap(), [Clear]);
    }

    #[test]
    fn errors() {
        assert!(parse("1 thru").is_err());
        assert!(parse("at 150").is_err());
        assert!(parse("banana").unwrap_err().contains("banana"));
        assert!(parse("r 5").is_err(), "r is ambiguous (record, release)");
        assert!(parse("1.0 at 50").is_err(), "cells count from 1");
        assert!(parse("gobo").is_err());
    }
}
