//! Editing several cues at once: what an edit changed on one cue, applied to others, and pasting
//! chosen properties from one cue onto others.
//!
//! Works on the cues' JSON form, so every field (present and future) takes part without a list
//! to keep up to date: an edit is the set of leaf values that differ, by path. The cue's id,
//! number and a group's children never travel; type settings travel only between cues of the
//! same type.

use crate::model::Cue;
use serde_json::{Map, Value};

/// One changed value: its path in the cue's JSON and the new value (None = the field went away,
/// e.g. an optional trigger turned off).
#[derive(Clone, PartialEq, Debug)]
pub struct Change {
    path: Vec<String>,
    value: Option<Value>,
}

/// Fields never copied from one cue to another.
const OWN: [&str; 2] = ["id", "number"];

fn json(c: &Cue) -> Value {
    serde_json::to_value(c.shallow_clone()).unwrap_or(Value::Null)
}

fn diff(path: &mut Vec<String>, a: &Value, b: &Value, out: &mut Vec<Change>) {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            for (k, vb) in y {
                path.push(k.clone());
                match x.get(k) {
                    Some(va) => diff(path, va, vb, out),
                    None => out.push(Change { path: path.clone(), value: Some(vb.clone()) }),
                }
                path.pop();
            }
            for k in x.keys().filter(|k| !y.contains_key(*k)) {
                path.push(k.clone());
                out.push(Change { path: path.clone(), value: None });
                path.pop();
            }
        }
        _ if a != b => out.push(Change { path: path.clone(), value: Some(b.clone()) }),
        _ => {}
    }
}

/// What turned `before` into `after` (the same cue, edited).
pub fn changes(before: &Cue, after: &Cue) -> Vec<Change> {
    let mut out = Vec::new();
    diff(&mut Vec::new(), &json(before), &json(after), &mut out);
    let same_kind = before.kind.tag() == after.kind.tag();
    out.retain(|c| match c.path.first().map(String::as_str) {
        Some(k) if OWN.contains(&k) => false,
        // A group's children are cues of their own.
        Some("kind") => same_kind && c.path.get(2).is_none_or(|f| f != "children"),
        Some(_) => true,
        None => false,
    });
    out
}

fn set(v: &mut Value, path: &[String], value: Option<&Value>) -> bool {
    let Some((last, parents)) = path.split_last() else { return false };
    let mut cur = v;
    for p in parents {
        let Value::Object(m) = cur else { return false };
        cur = m.entry(p.clone()).or_insert_with(|| Value::Object(Map::new()));
    }
    let Value::Object(m) = cur else { return false };
    match value {
        Some(x) => m.insert(last.clone(), x.clone()).as_ref() != Some(x),
        None => m.remove(last).is_some(),
    }
}

/// Apply `changes` to `to` (type settings only if it is the same type as the edited cue was:
/// `kind_of` = that cue). Returns whether `to` changed.
pub fn apply(changes: &[Change], kind_of: &Cue, to: &mut Cue) -> bool {
    let same_kind = kind_of.kind.tag() == to.kind.tag();
    let mut v = json(to);
    let mut any = false;
    for c in changes {
        if c.path[0] == "kind" && !same_kind {
            continue;
        }
        any |= set(&mut v, &c.path, c.value.as_ref());
    }
    if !any {
        return false;
    }
    match serde_json::from_value::<Cue>(v) {
        Ok(new) if new.shallow_eq(to) => false,
        Ok(new) => {
            to.assign_shallow(new);
            true
        }
        Err(_) => false,
    }
}

/// Properties that can be pasted from one cue onto others.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Prop {
    /// Name.
    Name,
    /// Notes.
    Notes,
    /// Row colour.
    Colour,
    /// Armed or not.
    Armed,
    /// Pre-wait.
    PreWait,
    /// Post-wait.
    PostWait,
    /// Continue mode.
    Continue,
    /// Hotkey, MIDI, timecode and wall-clock triggers and the second-trigger action.
    Triggers,
    /// Every type setting except the media file and the target (same type only).
    TypeSettings,
    /// Media file (same type only).
    MediaFile,
    /// Target cue (same type only).
    Target,
}

impl Prop {
    /// Every property, in the order the paste window lists them.
    pub const ALL: [Prop; 11] = [
        Prop::Name,
        Prop::Notes,
        Prop::Colour,
        Prop::Armed,
        Prop::PreWait,
        Prop::PostWait,
        Prop::Continue,
        Prop::Triggers,
        Prop::TypeSettings,
        Prop::MediaFile,
        Prop::Target,
    ];

    /// Name shown in the UI.
    pub fn label(self) -> &'static str {
        use crate::i18n::t;
        match self {
            Prop::Name => t("Name"),
            Prop::Notes => t("Notes"),
            Prop::Colour => t("Color"),
            Prop::Armed => t("Armed"),
            Prop::PreWait => t("Pre-wait"),
            Prop::PostWait => t("Post-wait"),
            Prop::Continue => t("Continue mode"),
            Prop::Triggers => t("Triggers"),
            Prop::TypeSettings => t("Type settings (same type only)"),
            Prop::MediaFile => t("Media file (same type only)"),
            Prop::Target => t("Target (same type only)"),
        }
    }

    /// Whether a change at `path` belongs to this property.
    fn covers(self, path: &[String]) -> bool {
        let top = path[0].as_str();
        // Type settings: kind → variant → field.
        let field = path.get(2).map(String::as_str);
        match self {
            Prop::Name => top == "name",
            Prop::Notes => top == "notes",
            Prop::Colour => top == "color",
            Prop::Armed => top == "armed",
            Prop::PreWait => top == "pre_wait",
            Prop::PostWait => top == "post_wait",
            Prop::Continue => top == "continue_mode",
            Prop::Triggers => matches!(top, "hotkey" | "timecode_trigger" | "midi_trigger" | "wallclock_trigger" | "second_trigger"),
            Prop::MediaFile => top == "kind" && field == Some("path"),
            Prop::Target => top == "kind" && field == Some("target"),
            Prop::TypeSettings => top == "kind" && !matches!(field, Some("path" | "target" | "children")),
        }
    }
}

/// Copy the chosen properties of `from` onto `to`. Returns whether `to` changed.
pub fn paste(from: &Cue, to: &mut Cue, props: &[Prop]) -> bool {
    // As if `to` had been edited into `from`: everything that differs, then only the chosen parts.
    let mut all = Vec::new();
    diff(&mut Vec::new(), &json(to), &json(from), &mut all);
    all.retain(|c| !OWN.contains(&c.path[0].as_str()) && props.iter().any(|p| p.covers(&c.path)));
    apply(&all, from, to)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AudioCue, ContinueMode, CueKind, KindTag, MidiTrigger};

    fn audio(path: &str, db: f32) -> Cue {
        Cue::new(CueKind::Audio(AudioCue { path: path.into(), volume_db: db, ..Default::default() }))
    }

    #[test]
    fn an_edit_carries_to_the_other_cues() {
        let a = audio("a.wav", 0.0);
        let mut edited = a.clone();
        edited.pre_wait = 2.5;
        if let CueKind::Audio(x) = &mut edited.kind {
            x.volume_db = -6.0;
        }
        edited.midi_trigger = Some(MidiTrigger::default());
        let ch = changes(&a, &edited);
        assert_eq!(ch.len(), 3, "{ch:?}");

        let mut b = audio("b.wav", -3.0);
        b.number = "7".into();
        assert!(apply(&ch, &edited, &mut b));
        assert_eq!(b.pre_wait, 2.5);
        assert!(b.midi_trigger.is_some());
        let CueKind::Audio(x) = &b.kind else { panic!() };
        assert_eq!((x.volume_db, x.path.to_str()), (-6.0, Some("b.wav")), "only what changed");
        assert_eq!(b.number, "7");

        // Another type takes the cue fields only.
        let mut w = Cue::new(KindTag::Wait.default_kind());
        let kind = w.kind.clone();
        assert!(apply(&ch, &edited, &mut w));
        assert_eq!((w.pre_wait, w.kind), (2.5, kind));

        // Turning the trigger off again removes it.
        let mut off = edited.clone();
        off.midi_trigger = None;
        assert!(apply(&changes(&edited, &off), &off, &mut b));
        assert!(b.midi_trigger.is_none());
    }

    #[test]
    fn group_children_stay() {
        let g = Cue::new(CueKind::Group { mode: Default::default(), children: vec![Cue::new(CueKind::Memo)] });
        let mut other = Cue::new(CueKind::Group { mode: Default::default(), children: vec![Cue::new(CueKind::Memo), Cue::new(CueKind::Memo)] });
        let mut edited = g.clone();
        edited.continue_mode = ContinueMode::AutoFollow;
        if let CueKind::Group { mode, .. } = &mut edited.kind {
            *mode = crate::model::GroupMode::Timeline;
        }
        assert!(apply(&changes(&g, &edited), &edited, &mut other));
        assert_eq!(other.children().len(), 2);
        assert!(matches!(other.kind, CueKind::Group { mode: crate::model::GroupMode::Timeline, .. }));
    }

    #[test]
    fn paste_takes_the_chosen_properties() {
        let mut from = audio("a.wav", -10.0);
        from.name = "Rain".into();
        from.pre_wait = 1.0;
        from.color = crate::model::CueColor::ALL[1];
        let mut to = audio("b.wav", 0.0);
        assert!(paste(&from, &mut to, &[Prop::PreWait, Prop::TypeSettings]));
        let CueKind::Audio(x) = &to.kind else { panic!() };
        assert_eq!((to.pre_wait, x.volume_db, x.path.to_str()), (1.0, -10.0, Some("b.wav")));
        assert!(to.name.is_empty() && to.color != from.color);
        assert!(paste(&from, &mut to, &[Prop::MediaFile, Prop::Name]));
        let CueKind::Audio(x) = &to.kind else { panic!() };
        assert_eq!((to.name.as_str(), x.path.to_str()), ("Rain", Some("a.wav")));
        assert!(!paste(&from, &mut to, &[Prop::Name]), "nothing left to change");
    }
}
