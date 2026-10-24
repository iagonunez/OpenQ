//! MIDI Show Control (MSC) in: SysEx commands from a stage manager's desk, a lighting console or
//! another show program, read on the control surface MIDI input ([`crate::surface::MidiSurface`]).
//!
//! MIDI cues send MSC too ([`encode`]).
//!
//! A message is `F0 7F <device> 02 <command format> <command> <data> F7`. OpnQ answers its own
//! device ID, its group ID (if set) and the all-call ID 7F, for one command format or any
//! ([`MscSettings`]). The data of the cue commands is a cue number, a cue list and a cue path, each
//! optional, ASCII, separated by 00 (`TIMED_GO` puts a time before them, which OpnQ skips).
//!
//! What each command does ([`commands`]):
//! * `GO` — with a cue number, puts the playhead on it and GOes when it is at the top of the current
//!   list, else starts it where it is; without one, the main GO (on the named list, brought forward).
//! * `STOP` / `RESUME` — pause / resume the cue, or everything (MSC's STOP holds a cue, it doesn't end it).
//! * `GO_OFF` — stop the cue (or Stop all). `ALL_OFF` — Stop all. `RESET` — stop at once, playhead to the top.
//! * `LOAD` — put the playhead on the cue (its list brought forward). `STANDBY_+` / `STANDBY_-` — move it.
//! * `OPEN_CUE_LIST` — bring the list forward.
//!
//! A cue list is named by its tab name or its tab number (from 1); without one the cue number is
//! looked for in every list, the current one first. Other commands (SET, FIRE, clocks, paths) are
//! refused with a message.

use crate::control::{Command, CueAction};
use crate::model::{Cue, CueId, CueList};
use serde::{Deserialize, Serialize};

/// The longest MSC message read (the standard's limit is 128 bytes).
pub const MAX_LEN: usize = 128;
/// The all-call device ID: every device answers it.
pub const ALL_CALL: u8 = 0x7f;
/// The all-types command format: every device answers it whatever its own format.
pub const ALL_TYPES: u8 = 0x7f;

/// How the show answers MSC (part of [`crate::surface::SurfaceSettings`]).
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
#[serde(default)]
pub struct MscSettings {
    /// MSC commands act on the show.
    pub enabled: bool,
    /// This device's ID (0..=111).
    pub device: u8,
    /// Group ID 1..=15 (sent as 70..7E), if the show belongs to one.
    pub group: Option<u8>,
    /// The command format answered besides all-types (None = any format).
    pub format: Option<u8>,
}

impl MscSettings {
    /// The message is for this device: its ID, its group or all-call, and a format it answers.
    pub fn answers(&self, m: &Msc) -> bool {
        let device = m.device == self.device || m.device == ALL_CALL || self.group.is_some_and(|g| (1..=15).contains(&g) && m.device == 0x6f + g);
        let format = self.format.is_none_or(|f| m.format == f || m.format == ALL_TYPES);
        self.enabled && device && format
    }
}

/// Command formats offered in the settings (code, name).
pub const FORMATS: [(u8, &str); 10] = [
    (0x01, "Lighting"),
    (0x02, "Moving lights"),
    (0x10, "Sound"),
    (0x11, "Music"),
    (0x20, "Machinery"),
    (0x30, "Video"),
    (0x40, "Projection"),
    (0x50, "Process control"),
    (0x60, "Pyro"),
    (0x7f, "All types"),
];

/// Name of a command format (translated), or its hex code.
pub fn format_label(f: u8) -> String {
    FORMATS.iter().find(|(c, _)| *c == f).map_or_else(|| format!("{f:02X}"), |(_, n)| crate::i18n::t(n).to_string())
}

/// An MSC command.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MscCommand {
    /// 01 GO.
    Go,
    /// 02 STOP.
    Stop,
    /// 03 RESUME.
    Resume,
    /// 04 TIMED_GO.
    TimedGo,
    /// 05 LOAD.
    Load,
    /// 08 ALL_OFF.
    AllOff,
    /// 0A RESET.
    Reset,
    /// 0B GO_OFF.
    GoOff,
    /// 11 STANDBY_+.
    StandbyNext,
    /// 12 STANDBY_-.
    StandbyPrevious,
    /// 1B OPEN_CUE_LIST.
    OpenCueList,
    /// Any other command (its code).
    Other(u8),
}

impl MscCommand {
    fn from_code(c: u8) -> Self {
        match c {
            0x01 => Self::Go,
            0x02 => Self::Stop,
            0x03 => Self::Resume,
            0x04 => Self::TimedGo,
            0x05 => Self::Load,
            0x08 => Self::AllOff,
            0x0a => Self::Reset,
            0x0b => Self::GoOff,
            0x11 => Self::StandbyNext,
            0x12 => Self::StandbyPrevious,
            0x1b => Self::OpenCueList,
            c => Self::Other(c),
        }
    }

    /// The standard's name ("GO", "STANDBY_+", …).
    pub fn name(self) -> String {
        match self {
            Self::Go => "GO".into(),
            Self::Stop => "STOP".into(),
            Self::Resume => "RESUME".into(),
            Self::TimedGo => "TIMED_GO".into(),
            Self::Load => "LOAD".into(),
            Self::AllOff => "ALL_OFF".into(),
            Self::Reset => "RESET".into(),
            Self::GoOff => "GO_OFF".into(),
            Self::StandbyNext => "STANDBY_+".into(),
            Self::StandbyPrevious => "STANDBY_-".into(),
            Self::OpenCueList => "OPEN_CUE_LIST".into(),
            Self::Other(c) => format!("command {c:02X}"),
        }
    }
}

/// One MSC message.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Msc {
    /// Device ID it is sent to.
    pub device: u8,
    /// Command format.
    pub format: u8,
    /// Command.
    pub command: MscCommand,
    /// Cue number.
    pub cue: Option<String>,
    /// Cue list.
    pub list: Option<String>,
    /// Cue path.
    pub path: Option<String>,
}

impl Msc {
    /// Short text for the UI and the log ("GO 12 · list 2").
    pub fn label(&self) -> String {
        let mut s = self.command.name();
        if let Some(c) = &self.cue {
            s += &format!(" {c}");
        }
        if let Some(l) = &self.list {
            s += &crate::tr!(" · list {list}", list = l.clone());
        }
        s
    }
}

/// Read an MSC message (None for any other MIDI message, or one that breaks the format).
pub fn parse(msg: &[u8]) -> Option<Msc> {
    if msg.len() > MAX_LEN || msg.len() < 6 {
        return None;
    }
    let [0xf0, 0x7f, device, 0x02, format, command, rest @ ..] = msg else { return None };
    let data = rest.strip_suffix(&[0xf7])?;
    if device & 0x80 != 0 || format & 0x80 != 0 || data.iter().any(|b| b & 0x80 != 0) {
        return None;
    }
    let command = MscCommand::from_code(*command);
    let data = match command {
        // hours/type, minutes, seconds, frames, subframes (or fewer: 5 bytes is the full form).
        MscCommand::TimedGo => data.get(5..).unwrap_or_default(),
        _ => data,
    };
    let mut fields = data.split(|b| *b == 0).map(|f| {
        let s: String = f.iter().map(|&b| b as char).collect::<String>().trim().to_string();
        (!s.is_empty() && s.chars().all(|c| c.is_ascii_graphic() || c == ' ')).then_some(s)
    });
    let (cue, list, path) = (fields.next().flatten(), fields.next().flatten(), fields.next().flatten());
    Some(Msc { device: *device, format: *format, command, cue, list, path })
}

/// Commands a MIDI cue sends (code, the standard's name).
pub const SEND_COMMANDS: [(u8, &str); 10] = [
    (0x01, "GO"),
    (0x02, "STOP"),
    (0x03, "RESUME"),
    (0x05, "LOAD"),
    (0x08, "ALL_OFF"),
    (0x0a, "RESET"),
    (0x0b, "GO_OFF"),
    (0x11, "STANDBY_+"),
    (0x12, "STANDBY_-"),
    (0x1b, "OPEN_CUE_LIST"),
];

/// An MSC message (the inverse of [`parse`]): cue number and list as ASCII, each optional; the list
/// is sent only with a cue number, as the standard orders them. Characters MSC can't carry are
/// dropped and the message is kept within [`MAX_LEN`].
pub fn encode(device: u8, format: u8, command: u8, cue: &str, list: &str) -> Vec<u8> {
    let field = |s: &str| -> Vec<u8> { s.chars().filter(|c| c.is_ascii_graphic() || *c == ' ').map(|c| c as u8).take(48).collect() };
    let mut out = vec![0xf0, 0x7f, device & 0x7f, 0x02, format & 0x7f, command & 0x7f];
    let (cue, list) = (field(cue.trim()), field(list.trim()));
    if !cue.is_empty() {
        out.extend(cue);
        if !list.is_empty() {
            out.push(0);
            out.extend(list);
        }
    }
    out.push(0xf7);
    out
}

/// The lists in tab order: (tab, cues).
fn tabs(cues: &CueList) -> Vec<(usize, &str, &[Cue])> {
    let mut out = vec![(cues.pos, cues.info.name.as_str(), cues.cues.as_slice())];
    for (i, o) in cues.others.iter().enumerate() {
        out.push((if i < cues.pos { i } else { i + 1 }, o.info.name.as_str(), o.cues.as_slice()));
    }
    out
}

/// The tab of list `name`: its tab name (any case), else its tab number from 1.
fn find_list(cues: &CueList, name: &str) -> Result<usize, String> {
    let tabs = tabs(cues);
    if let Some(t) = tabs.iter().find(|t| t.1.eq_ignore_ascii_case(name)) {
        return Ok(t.0);
    }
    name.parse::<usize>().ok().filter(|n| (1..=tabs.len()).contains(n)).map(|n| n - 1).ok_or_else(|| crate::tr!("no cue list {list}", list = name.to_string()))
}

/// The first cue numbered `number` in `list` (at any depth), and whether it is at its top level.
fn find_in(list: &[Cue], number: &str) -> Option<(CueId, bool)> {
    if let Some(c) = list.iter().find(|c| c.number == number) {
        return Some((c.id, true));
    }
    let mut found = None;
    for c in list {
        c.visit(&mut |c| {
            if found.is_none() && c.number == number {
                found = Some(c.id);
            }
        });
    }
    found.map(|id| (id, false))
}

/// The cue numbered `number`: (its tab, id, at the top level), in `list` or else any list (current first).
fn find_cue(cues: &CueList, number: &str, list: Option<usize>) -> Result<(usize, CueId, bool), String> {
    tabs(cues)
        .into_iter()
        .filter(|t| list.is_none_or(|l| l == t.0))
        .find_map(|(tab, _, cues)| find_in(cues, number).map(|(id, top)| (tab, id, top)))
        .ok_or_else(|| crate::tr!("no cue numbered {number}", number = number.to_string()))
}

/// The commands for an MSC message, in order.
pub fn commands(cues: &CueList, m: &Msc) -> Result<Vec<Command>, String> {
    let list = m.list.as_deref().map(|l| find_list(cues, l)).transpose()?;
    let cue = m.cue.as_deref().map(|n| find_cue(cues, n, list)).transpose()?;
    let current = cues.pos;
    // Bring a list forward unless it already is.
    let switch = |tab: usize| (tab != current).then_some(Command::SwitchList(tab));
    Ok(match (m.command, cue) {
        (MscCommand::Go | MscCommand::TimedGo, Some((tab, id, top))) => {
            if tab == current && top {
                vec![Command::SetPlayhead(Some(id)), Command::Go]
            } else {
                vec![Command::Cue(id, CueAction::Start)]
            }
        }
        (MscCommand::Go | MscCommand::TimedGo, None) => list.and_then(switch).into_iter().chain([Command::Go]).collect(),
        (MscCommand::Stop, Some((_, id, _))) => vec![Command::Cue(id, CueAction::Pause)],
        (MscCommand::Stop, None) => vec![Command::PauseAll],
        (MscCommand::Resume, Some((_, id, _))) => vec![Command::Cue(id, CueAction::Resume)],
        (MscCommand::Resume, None) => vec![Command::ResumeAll],
        (MscCommand::GoOff, Some((_, id, _))) => vec![Command::Cue(id, CueAction::Stop)],
        (MscCommand::GoOff | MscCommand::AllOff, None) => vec![Command::Stop],
        (MscCommand::AllOff, Some(_)) => vec![Command::Stop],
        (MscCommand::Reset, _) => vec![Command::Panic, Command::SetPlayhead(cues.first())],
        (MscCommand::Load, Some((tab, id, _))) => switch(tab).into_iter().chain([Command::SetPlayhead(Some(id))]).collect(),
        (MscCommand::Load, None) => return Err(crate::i18n::t("LOAD needs a cue number").into()),
        (MscCommand::StandbyNext, _) => list.and_then(switch).into_iter().chain([Command::PlayheadNext]).collect(),
        (MscCommand::StandbyPrevious, _) => list.and_then(switch).into_iter().chain([Command::PlayheadPrevious]).collect(),
        (MscCommand::OpenCueList, _) => match list.or(cue.map(|c| c.0)) {
            Some(tab) => switch(tab).into_iter().collect(),
            None => return Err(crate::i18n::t("OPEN_CUE_LIST needs a cue list").into()),
        },
        (MscCommand::Other(_), _) => return Err(crate::i18n::t("not supported").into()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a MIDI cue sends reads back as the same command.
    #[test]
    fn encoded_messages_parse_back() {
        let m = parse(&encode(0x7f, 0x10, 0x01, "12.5", "2")).unwrap();
        assert_eq!((m.device, m.format, m.command, m.cue.as_deref(), m.list.as_deref()), (0x7f, 0x10, MscCommand::Go, Some("12.5"), Some("2")));
        assert_eq!(encode(1, 1, 0x08, "", "list"), vec![0xf0, 0x7f, 1, 2, 1, 8, 0xf7], "no list without a cue");
        assert!(encode(1, 1, 1, &"9".repeat(500), &"x".repeat(500)).len() <= MAX_LEN);
        use crate::model::{MidiCue, MidiMessageKind, parse_sysex};
        let cue = |message, value| MidiCue { message, channel: 9, number: 36, value, ..Default::default() };
        assert_eq!(cue(MidiMessageKind::NoteOn, 100).bytes().unwrap(), vec![0x99, 36, 100]);
        assert_eq!(cue(MidiMessageKind::Program, 0).bytes().unwrap(), vec![0xc9, 36]);
        assert_eq!(cue(MidiMessageKind::PitchBend, 8192).bytes().unwrap(), vec![0xe9, 0, 64]);
        assert_eq!(parse_sysex("f0 7e 7f 06 01 f7").unwrap(), vec![0xf0, 0x7e, 0x7f, 6, 1, 0xf7]);
        assert!(parse_sysex("F0 80 F7").is_err() && parse_sysex("7E F7").is_err() && parse_sysex("F0 7").is_err());
    }
    use crate::model::{CueKind, OtherList};

    fn msg(command: u8, data: &[u8]) -> Vec<u8> {
        [&[0xf0, 0x7f, 0x01, 0x02, 0x10, command][..], data, &[0xf7]].concat()
    }

    fn cue(number: &str) -> Cue {
        let mut c = Cue::new(CueKind::Memo);
        c.number = number.into();
        c
    }

    fn show() -> CueList {
        let mut l = CueList::default();
        l.info.name = "Main".into();
        l.cues = vec![cue("1"), cue("2.5")];
        let mut other = OtherList::default();
        other.info.name = "Sound".into();
        other.cues = vec![cue("1"), cue("7")];
        l.others.push(other);
        l
    }

    #[test]
    fn parses_cue_list_and_path() {
        let m = parse(&msg(0x01, b"2.5\0Sound\0x")).unwrap();
        assert_eq!((m.device, m.format, m.command), (1, 0x10, MscCommand::Go));
        assert_eq!((m.cue.as_deref(), m.list.as_deref(), m.path.as_deref()), (Some("2.5"), Some("Sound"), Some("x")));
        let m = parse(&msg(0x01, b"")).unwrap();
        assert_eq!((m.cue, m.list), (None, None));
        // TIMED_GO: the time comes first.
        let m = parse(&msg(0x04, &[0x61, 0, 2, 0, 0, b'9'])).unwrap();
        assert_eq!((m.command, m.cue.as_deref()), (MscCommand::TimedGo, Some("9")));
        // Not MSC, broken, too long.
        assert!(parse(&[0xf0, 0x7f, 0x01, 0x01, 0x01, 0x00, 0xf7]).is_none(), "MTC full frame");
        assert!(parse(&msg(0x01, b"12")[..8]).is_none(), "no F7");
        assert!(parse(&msg(0x01, &[0x81])).is_none());
        assert!(parse(&msg(0x01, &[b'1'; 200])).is_none());
        assert!(parse(&[0x90, 1, 2]).is_none());
    }

    #[test]
    fn answers_its_ids_and_format() {
        let s = MscSettings { enabled: true, device: 1, group: Some(2), format: Some(0x10) };
        let m = |device, format| Msc { device, format, command: MscCommand::Go, cue: None, list: None, path: None };
        assert!(s.answers(&m(1, 0x10)));
        assert!(s.answers(&m(0x7f, 0x7f)));
        assert!(s.answers(&m(0x71, 0x10)), "group 2");
        assert!(!s.answers(&m(0x70, 0x10)), "group 1");
        assert!(!s.answers(&m(2, 0x10)));
        assert!(!s.answers(&m(1, 0x01)), "lighting");
        assert!(MscSettings { format: None, ..s.clone() }.answers(&m(1, 0x01)));
        assert!(!MscSettings { enabled: false, ..s }.answers(&m(1, 0x10)));
    }

    #[test]
    fn commands_find_cues_and_lists() {
        let l = show();
        let run = |command, data: &[u8]| commands(&l, &parse(&msg(command, data)).unwrap());
        let id = |tab: usize, i: usize| if tab == 0 { l.cues[i].id } else { l.others[0].cues[i].id };
        assert_eq!(run(0x01, b"2.5").unwrap(), vec![Command::SetPlayhead(Some(id(0, 1))), Command::Go]);
        assert_eq!(run(0x01, b"").unwrap(), vec![Command::Go]);
        // Current list first; another list's cue starts where it is.
        assert_eq!(run(0x01, b"1").unwrap()[0], Command::SetPlayhead(Some(id(0, 0))));
        assert_eq!(run(0x01, b"1\0Sound").unwrap(), vec![Command::Cue(id(1, 0), CueAction::Start)]);
        assert_eq!(run(0x01, b"7\x002").unwrap(), vec![Command::Cue(id(1, 1), CueAction::Start)], "list by tab number");
        assert_eq!(run(0x01, b"\0sound").unwrap(), vec![Command::SwitchList(1), Command::Go]);
        assert_eq!(run(0x02, b"").unwrap(), vec![Command::PauseAll]);
        assert_eq!(run(0x02, b"7").unwrap(), vec![Command::Cue(id(1, 1), CueAction::Pause)]);
        assert_eq!(run(0x03, b"").unwrap(), vec![Command::ResumeAll]);
        assert_eq!(run(0x0b, b"2.5").unwrap(), vec![Command::Cue(id(0, 1), CueAction::Stop)]);
        assert_eq!(run(0x08, b"").unwrap(), vec![Command::Stop]);
        assert_eq!(run(0x0a, b"").unwrap(), vec![Command::Panic, Command::SetPlayhead(Some(id(0, 0)))]);
        assert_eq!(run(0x05, b"7").unwrap(), vec![Command::SwitchList(1), Command::SetPlayhead(Some(id(1, 1)))]);
        assert_eq!(run(0x11, b"").unwrap(), vec![Command::PlayheadNext]);
        assert_eq!(run(0x1b, b"\0Sound").unwrap(), vec![Command::SwitchList(1)]);
        assert!(run(0x01, b"99").is_err());
        assert!(run(0x01, b"1\0Lights").is_err());
        assert!(run(0x05, b"").is_err());
        assert!(run(0x07, b"1").is_err(), "FIRE");
    }

    #[test]
    fn tabs_follow_the_current_position() {
        let mut l = show();
        l.pos = 1;
        // Tab 1 is now the background list, tab 2 the current one.
        assert_eq!(commands(&l, &parse(&msg(0x1b, b"\x001")).unwrap()).unwrap(), vec![Command::SwitchList(0)]);
        assert_eq!(commands(&l, &parse(&msg(0x1b, b"\x002")).unwrap()).unwrap(), vec![]);
    }
}
