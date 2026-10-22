//! Show warnings: cues that won't do what they look like they do (no file, a missing target, a
//! message that can't be sent...), found before the show instead of during it. Pure: file checks
//! come from the caller (the UI asks the cached [`crate::media::Prober`]), so it runs every few
//! frames without touching the disk.

use crate::model::{Cue, CueId, CueKind, GroupMode, LightAction, TimecodeSource, Workspace};
use std::collections::HashMap;
use std::path::Path;

/// How bad a problem is.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Severity {
    /// The cue can't do its job (the cue list shows it in red).
    Error,
    /// Likely a mistake, but the cue runs.
    Warning,
}

/// One problem with one cue.
#[derive(Clone, PartialEq, Debug)]
pub struct Warning {
    /// The cue.
    pub cue: CueId,
    /// Tab of the cue list it is in.
    pub tab: usize,
    /// How bad.
    pub severity: Severity,
    /// What is wrong, for the user.
    pub text: String,
}

/// What the caller knows about a media file.
#[derive(Clone, PartialEq, Debug)]
pub enum FileState {
    /// Exists and reads fine (or not probed yet).
    Fine,
    /// Not there.
    Missing,
    /// There but it can't be read.
    Unreadable(String),
}

/// Whether this kind of cue does nothing without a target (Stop and Pause without one act on
/// everything).
pub fn needs_target(kind: &CueKind) -> bool {
    matches!(
        kind,
        CueKind::Fade(_)
            | CueKind::Start { .. }
            | CueKind::Goto { .. }
            | CueKind::Devamp { .. }
            | CueKind::Arm { .. }
            | CueKind::Disarm { .. }
            | CueKind::Reset { .. }
            | CueKind::Load { .. }
            | CueKind::Target { .. }
    )
}

/// A cue that needs a target but has none, or whose target was deleted.
pub fn target_missing(kind: &CueKind, ws: &Workspace) -> bool {
    match kind.target() {
        Some(t) => ws.cue_list.find(t).is_none(),
        None => needs_target(kind),
    }
}

/// Every problem in the show, in cue list order (all lists, groups included).
pub fn check(ws: &Workspace, file: &dyn Fn(&Path) -> FileState) -> Vec<Warning> {
    let mut out = Vec::new();
    let mut hotkeys: HashMap<String, Vec<CueId>> = HashMap::new();
    let mut numbers: HashMap<(usize, &str), Vec<CueId>> = HashMap::new();
    let cl = &ws.cue_list;
    let current = cl.pos.min(cl.others.len());
    let mut tabs: HashMap<CueId, usize> = HashMap::new();
    for (li, (info, cues)) in cl.lists().into_iter().enumerate() {
        // `lists` gives the current list first, then the others in tab order without it.
        let tab = match li {
            0 => current,
            j if j - 1 < current => j - 1,
            j => j,
        };
        let mut stack: Vec<(&Cue, Option<&Cue>)> = cues.iter().rev().map(|c| (c, None)).collect();
        while let Some((c, parent)) = stack.pop() {
            tabs.insert(c.id, tab);
            let mut add = |severity, text: String| out.push(Warning { cue: c.id, tab, severity, text });
            cue(ws, c, file, &mut add);
            if c.timecode_trigger.is_some() && info.timecode.source == TimecodeSource::Off {
                add(Severity::Warning, crate::i18n::t("Has a timecode trigger, but its cue list doesn't follow timecode").into());
            }
            if let Some(p) = parent
                && matches!(p.kind, CueKind::Group { mode: GroupMode::Timeline, .. })
                && c.continue_mode != crate::model::ContinueMode::DoNotContinue
            {
                add(Severity::Warning, crate::i18n::t("Its continue mode is ignored inside a timeline group").into());
            }
            if let Some(k) = &c.hotkey {
                hotkeys.entry(k.to_lowercase()).or_default().push(c.id);
            }
            if !c.number.is_empty() {
                numbers.entry((tab, c.number.as_str())).or_default().push(c.id);
            }
            stack.extend(c.children().iter().rev().map(|ch| (ch, Some(c))));
        }
    }
    let mut add_shared = |ids: &[CueId], text: String| {
        for id in ids {
            out.push(Warning { cue: *id, tab: tabs.get(id).copied().unwrap_or(0), severity: Severity::Warning, text: text.clone() });
        }
    };
    let mut keys: Vec<_> = hotkeys.into_iter().filter(|(_, ids)| ids.len() > 1).collect();
    keys.sort();
    for (k, ids) in keys {
        add_shared(&ids, crate::tr!("{n} cues share the hotkey {key}: it fires them all", n = ids.len(), key = k.to_uppercase()));
    }
    let mut nums: Vec<_> = numbers.into_iter().filter(|(_, ids)| ids.len() > 1).collect();
    nums.sort();
    for ((_, n), ids) in nums {
        add_shared(&ids, crate::tr!("Cue number {number} is used {n} times in its list: OSC, MIDI Show Control and remote devices pick the first", number = n, n = ids.len()));
    }
    out
}

/// One cue's own problems.
fn cue(ws: &Workspace, c: &Cue, file: &dyn Fn(&Path) -> FileState, add: &mut dyn FnMut(Severity, String)) {
    use crate::i18n::t;
    use Severity::*;
    let patch = |p: u8, add: &mut dyn FnMut(Severity, String)| {
        if p == 0 {
            return;
        }
        match ws.settings.patches.get(p as usize - 1) {
            None => add(Error, crate::tr!("Plays through audio patch {n}, which doesn't exist", n = p)),
            Some(a) if a.device.is_none() => add(Warning, crate::tr!("Audio patch \"{name}\" has no output device chosen (it plays on the main output)", name = a.name)),
            Some(_) => {}
        }
    };
    if let Some(p) = c.kind.media_path() {
        if p.as_os_str().is_empty() {
            add(Error, t("No media file").into());
        } else {
            match file(p) {
                FileState::Fine => {}
                FileState::Missing => add(Error, crate::tr!("Media file not found: {path}", path = p.display())),
                FileState::Unreadable(e) => add(Error, crate::tr!("Media file can't be read: {error}", error = e)),
            }
        }
    }
    if target_missing(&c.kind, ws) {
        add(Error, if c.kind.target().is_some() { t("Its target cue was deleted") } else { t("No target cue") }.into());
    }
    match &c.kind {
        CueKind::Audio(a) => patch(a.patch, add),
        CueKind::Video(v) => patch(v.patch, add),
        CueKind::Mic(m) => patch(m.patch, add),
        CueKind::Target { new_target: Some(n), .. } if ws.cue_list.find(*n).is_none() => add(Error, t("The new target cue was deleted").into()),
        CueKind::Fade(f) => {
            if let Some(tg) = f.target.and_then(|t| ws.cue_list.find(t))
                && !matches!(
                    tg.kind,
                    CueKind::Audio(_) | CueKind::Video(_) | CueKind::Text(_) | CueKind::Mic(_) | CueKind::Camera(_) | CueKind::Group { .. }
                )
            {
                add(Error, t("Its target has nothing to fade (sound or pictures)").into());
            }
        }
        CueKind::Devamp { target: Some(tg) } => {
            if ws.cue_list.find(*tg).is_some_and(|c| !matches!(c.kind, CueKind::Audio(_))) {
                add(Warning, t("Devamp acts on Audio cues only").into());
            }
        }
        CueKind::Network(n) => {
            if n.host.trim().is_empty() {
                add(Error, t("No destination host").into());
            }
            if !n.address.starts_with('/') {
                add(Error, t("The OSC address must start with /").into());
            }
        }
        CueKind::Midi(m) => {
            if let Err(e) = m.bytes() {
                add(Error, crate::tr!("MIDI message can't be sent: {error}", error = e));
            }
        }
        CueKind::Light(l) if l.action == LightAction::Go => match l.cue {
            None => add(Error, t("No lighting cue chosen").into()),
            Some(id) if ws.lighting.find_cue(id).is_none() => add(Error, t("Its lighting cue was deleted").into()),
            Some(_) => {}
        },
        CueKind::Light(l) => {
            if let Some(s) = l.sequence
                && !ws.lighting.sequences.iter().any(|q| q.id == s)
            {
                add(Error, t("The sequence it releases was deleted").into());
            }
        }
        CueKind::Group { children, .. } if children.is_empty() => add(Warning, t("Empty group").into()),
        _ => {}
    }
    if let Some(k) = &c.hotkey {
        if egui::Key::from_name(k).is_none() {
            add(Error, crate::tr!("Hotkey \"{key}\" is not a key name", key = k));
        } else if matches!(k.to_lowercase().as_str(), "space" | "p" | "r" | "escape" | "f1") {
            add(Error, crate::tr!("Hotkey {key} is taken by GO, Pause, Resume, Stop all or the user manual (F1)", key = k));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AudioCue, KindTag, NetworkCue};

    fn texts(ws: &Workspace, file: &dyn Fn(&Path) -> FileState) -> Vec<(CueId, Severity)> {
        check(ws, file).into_iter().map(|w| (w.cue, w.severity)).collect()
    }

    #[test]
    fn finds_broken_cues() {
        let fine = |_: &Path| FileState::Fine;
        let mut ws = Workspace::default();
        let audio = Cue::new(CueKind::Audio(AudioCue { path: "a.wav".into(), ..Default::default() }));
        let no_file = Cue::new(KindTag::Audio.default_kind());
        let fade = Cue::new(KindTag::Fade.default_kind());
        let mut start = Cue::new(KindTag::Start.default_kind());
        *start.kind.target_mut().unwrap() = Some(uuid::Uuid::new_v4());
        let stop = Cue::new(KindTag::Stop.default_kind());
        let net = Cue::new(CueKind::Network(NetworkCue { host: " ".into(), ..Default::default() }));
        let mut k1 = Cue::new(CueKind::Memo);
        k1.hotkey = Some("F1".into());
        let mut k2 = Cue::new(CueKind::Memo);
        k2.hotkey = Some("f1".into());
        let mut space = Cue::new(CueKind::Memo);
        space.hotkey = Some("Space".into());
        let ids = [audio.id, no_file.id, fade.id, start.id, stop.id, net.id, k1.id, k2.id, space.id];
        ws.cue_list.cues = vec![audio, no_file, fade, start, stop, net, k1, k2, space];
        let found = texts(&ws, &fine);
        let has = |id: CueId, s: Severity| found.contains(&(id, s));
        assert!(!found.iter().any(|w| w.0 == ids[0]), "{found:?}");
        assert!(has(ids[1], Severity::Error), "no file");
        assert!(has(ids[2], Severity::Error), "fade without a target");
        assert!(has(ids[3], Severity::Error), "deleted target");
        assert!(!found.iter().any(|w| w.0 == ids[4]), "Stop without a target stops everything");
        assert!(has(ids[5], Severity::Error), "no host");
        assert!(has(ids[6], Severity::Warning) && has(ids[7], Severity::Warning), "shared hotkey");
        assert!(has(ids[8], Severity::Error), "reserved hotkey");
        let missing = |_: &Path| FileState::Missing;
        assert!(texts(&ws, &missing).contains(&(ids[0], Severity::Error)));
    }

    #[test]
    fn timeline_children_with_continue_modes_and_patches() {
        let mut child = Cue::new(CueKind::Audio(AudioCue { path: "a.wav".into(), patch: 2, ..Default::default() }));
        child.continue_mode = crate::model::ContinueMode::AutoContinue;
        let id = child.id;
        let mut ws = Workspace::default();
        ws.cue_list.cues = vec![Cue::new(CueKind::Group { mode: GroupMode::Timeline, children: vec![child] })];
        let w = check(&ws, &|_| FileState::Fine);
        assert_eq!(w.iter().filter(|w| w.cue == id).count(), 2, "{w:?}");
    }
}
