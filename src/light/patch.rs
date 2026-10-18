//! Lighting design deltas for remote devices: [`diff`] two designs into a [`LightingPatch`] that
//! holds only what changed (fixtures, types, groups, presets, focus points, effects by id; sequences down to
//! their cues), [`apply`] it to a copy of the old design.
//!
//! A drag on an encoder changes one cue or one programmer-stored preset, so the patch is a few
//! hundred bytes where the design can be hundreds of MB. Comparing is cheap: unchanged cue and
//! preset values are the same allocation (`model::Shared`), so `==` stops at a pointer compare.
//! Lists with duplicate ids (a patch with a number clash) go whole.

use super::dmx::OutputConfig;
use super::dmxin::InputConfig;
use super::effects::Effect;
use super::model::{Fixture, FixtureType, FocusPoint, Group, LightCue, Lighting, PlotShape, Preset, Sequence};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::hash::Hash;

/// Changes to a list of items with ids.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum ListPatch<K, T> {
    /// Same ids in the same order; these items changed.
    Changed(Vec<T>),
    /// The ids changed (added, removed, reordered): the new order, and the items that are new
    /// or changed. The others are taken from the old list.
    Reordered(Vec<K>, Vec<T>),
    /// The whole list (duplicate ids).
    Whole(Vec<T>),
}

fn unique<K: Eq + Hash, T>(items: &[T], key: &impl Fn(&T) -> K) -> bool {
    let mut seen = HashSet::with_capacity(items.len());
    items.iter().all(|t| seen.insert(key(t)))
}

/// The changes from `old` to `new` (None if equal).
fn diff_list<K, T>(old: &[T], new: &[T], key: impl Fn(&T) -> K) -> Option<ListPatch<K, T>>
where
    K: Eq + Hash + Clone,
    T: Clone + PartialEq,
{
    if old.len() == new.len() && old.iter().zip(new).all(|(a, b)| key(a) == key(b)) {
        let changed: Vec<T> = old.iter().zip(new).filter(|(a, b)| a != b).map(|(_, b)| b.clone()).collect();
        if changed.is_empty() {
            return None;
        }
        return Some(if unique(new, &key) { ListPatch::Changed(changed) } else { ListPatch::Whole(new.to_vec()) });
    }
    if !unique(old, &key) || !unique(new, &key) {
        return Some(ListPatch::Whole(new.to_vec()));
    }
    let before: HashMap<K, &T> = old.iter().map(|t| (key(t), t)).collect();
    let changed = new.iter().filter(|t| before.get(&key(t)) != Some(t)).cloned().collect();
    Some(ListPatch::Reordered(new.iter().map(&key).collect(), changed))
}

/// Apply a [`diff_list`] patch. False (list untouched) if it doesn't fit: an id it needs isn't there.
fn apply_list<K, T>(list: &mut Vec<T>, patch: &ListPatch<K, T>, key: impl Fn(&T) -> K) -> bool
where
    K: Eq + Hash + Clone,
    T: Clone,
{
    match patch {
        ListPatch::Whole(v) => *list = v.clone(),
        ListPatch::Changed(items) => {
            let mut at: HashMap<K, usize> = HashMap::with_capacity(list.len());
            for (i, t) in list.iter().enumerate() {
                at.insert(key(t), i);
            }
            let Some(places) = items.iter().map(|t| at.get(&key(t)).copied()).collect::<Option<Vec<usize>>>() else { return false };
            for (i, t) in places.into_iter().zip(items) {
                list[i] = t.clone();
            }
        }
        ListPatch::Reordered(order, items) => {
            let fresh: HashMap<K, &T> = items.iter().map(|t| (key(t), t)).collect();
            let mut old: HashMap<K, T> = HashMap::with_capacity(list.len());
            for t in list.iter() {
                old.insert(key(t), t.clone());
            }
            let mut out = Vec::with_capacity(order.len());
            for k in order {
                match fresh.get(k) {
                    Some(t) => out.push((*t).clone()),
                    None => match old.remove(k) {
                        Some(t) => out.push(t),
                        None => return false,
                    },
                }
            }
            *list = out;
        }
    }
    true
}

/// A sequence's own settings and the changes to its cues.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct SequencePatch {
    /// The sequence's settings with no cues (None: unchanged).
    pub head: Option<Sequence>,
    /// Sequence id.
    pub id: super::model::SequenceId,
    /// Cue changes (None: unchanged).
    pub cues: Option<ListPatch<super::model::LightCueId, LightCue>>,
}

/// The sequence without its cues (every other field, so a new field can't be missed).
fn head(s: &Sequence) -> Sequence {
    let Sequence { id, name, cues: _, move_in_black, mib_delay, mib_fade } = s;
    Sequence { id: *id, name: name.clone(), cues: Default::default(), move_in_black: *move_in_black, mib_delay: *mib_delay, mib_fade: *mib_fade }
}

/// Changes to the sequences: new ones whole, existing ones as [`SequencePatch`]es.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum SequencesPatch {
    /// Same ids in the same order.
    Changed(Vec<SequencePatch>),
    /// New order, the new sequences, and the changes to sequences kept.
    Reordered(Vec<super::model::SequenceId>, Vec<Sequence>, Vec<SequencePatch>),
    /// The whole list (duplicate ids).
    Whole(Vec<Sequence>),
}

fn diff_sequence(a: &Sequence, b: &Sequence) -> Option<SequencePatch> {
    let head = (head(a) != head(b)).then(|| head(b));
    let cues = diff_list(&a.cues, &b.cues, |c| c.id);
    (head.is_some() || cues.is_some()).then_some(SequencePatch { head, id: b.id, cues })
}

fn apply_sequence(s: &mut Sequence, p: &SequencePatch) -> bool {
    if let Some(c) = &p.cues
        && !apply_list(&mut s.cues, c, |c| c.id)
    {
        return false;
    }
    if let Some(h) = &p.head {
        let cues = std::mem::take(&mut s.cues);
        *s = Sequence { cues, ..h.clone() };
    }
    true
}

fn diff_sequences(old: &[Sequence], new: &[Sequence]) -> Option<SequencesPatch> {
    let key = |s: &Sequence| s.id;
    if !unique(old, &key) || !unique(new, &key) {
        return (old != new).then(|| SequencesPatch::Whole(new.to_vec()));
    }
    if old.len() == new.len() && old.iter().zip(new).all(|(a, b)| a.id == b.id) {
        let changed: Vec<SequencePatch> = old.iter().zip(new).filter_map(|(a, b)| diff_sequence(a, b)).collect();
        return (!changed.is_empty()).then_some(SequencesPatch::Changed(changed));
    }
    let before: HashMap<_, &Sequence> = old.iter().map(|s| (s.id, s)).collect();
    let mut added = Vec::new();
    let mut changed = Vec::new();
    for s in new {
        match before.get(&s.id) {
            Some(o) => changed.extend(diff_sequence(o, s)),
            None => added.push(s.clone()),
        }
    }
    Some(SequencesPatch::Reordered(new.iter().map(|s| s.id).collect(), added, changed))
}

fn apply_sequences(list: &mut Vec<Sequence>, p: &SequencesPatch) -> bool {
    let patch_all = |list: &mut Vec<Sequence>, changed: &[SequencePatch]| {
        changed.iter().all(|c| list.iter_mut().find(|s| s.id == c.id).is_some_and(|s| apply_sequence(s, c)))
    };
    match p {
        SequencesPatch::Whole(v) => {
            *list = v.clone();
            true
        }
        SequencesPatch::Changed(changed) => {
            let mut copy = list.clone();
            let ok = patch_all(&mut copy, changed);
            if ok {
                *list = copy;
            }
            ok
        }
        SequencesPatch::Reordered(order, added, changed) => {
            let mut old: HashMap<_, Sequence> = list.iter().map(|s| (s.id, s.clone())).collect();
            let mut out = Vec::with_capacity(order.len());
            for id in order {
                match added.iter().find(|s| s.id == *id) {
                    Some(s) => out.push(s.clone()),
                    None => match old.remove(id) {
                        Some(s) => out.push(s),
                        None => return false,
                    },
                }
            }
            if !patch_all(&mut out, changed) {
                return false;
            }
            *list = out;
            true
        }
    }
}

/// Changes between two lighting designs. Every field None = no change.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct LightingPatch {
    /// Fixture types.
    pub fixture_types: Option<ListPatch<String, FixtureType>>,
    /// Patched fixtures.
    pub fixtures: Option<ListPatch<super::model::FixtureId, Fixture>>,
    /// Groups.
    pub groups: Option<ListPatch<super::model::GroupId, Group>>,
    /// Sequences and their cues.
    pub sequences: Option<SequencesPatch>,
    /// Outputs (whole).
    pub outputs: Option<Vec<OutputConfig>>,
    /// Inputs (whole).
    pub inputs: Option<Vec<InputConfig>>,
    /// Plot bars and rectangles.
    pub shapes: Option<ListPatch<u32, PlotShape>>,
    /// Plot editor changes (whole).
    pub plot: Option<super::model::PlotLayout>,
    /// Presets.
    pub presets: Option<ListPatch<super::model::PresetId, Preset>>,
    /// Focus points.
    pub points: Option<ListPatch<super::model::PointId, FocusPoint>>,
    /// Effects pool.
    pub effects: Option<ListPatch<super::effects::EffectId, Effect>>,
    /// Playback buttons.
    pub buttons: Option<ListPatch<super::model::ButtonId, super::model::LightButton>>,
    /// Names of the playback button pages (whole).
    pub button_pages: Option<Vec<String>>,
    /// Size of the playback buttons.
    pub button_scale: Option<f32>,
    /// Stop all releases the lighting.
    pub stop_all_releases: Option<bool>,
    /// Release fade.
    pub release_fade: Option<f64>,
}

/// What changed from `old` to `new`; None if they are equal.
pub fn diff(old: &Lighting, new: &Lighting) -> Option<LightingPatch> {
    let changed = |a: bool| a.then_some(());
    let p = LightingPatch {
        fixture_types: diff_list(&old.fixture_types, &new.fixture_types, |t| t.id.clone()),
        fixtures: diff_list(&old.fixtures, &new.fixtures, |f| f.id),
        groups: diff_list(&old.groups, &new.groups, |g| g.id),
        sequences: diff_sequences(&old.sequences, &new.sequences),
        outputs: changed(old.outputs != new.outputs).map(|_| new.outputs.clone()),
        inputs: changed(old.inputs != new.inputs).map(|_| new.inputs.clone()),
        shapes: diff_list(&old.shapes, &new.shapes, |s| s.id),
        plot: changed(old.plot != new.plot).map(|_| new.plot.clone()),
        presets: diff_list(&old.presets, &new.presets, |p| p.id),
        points: diff_list(&old.points, &new.points, |p| p.id),
        effects: diff_list(&old.effects, &new.effects, |e| e.id),
        buttons: diff_list(&old.buttons, &new.buttons, |b| b.id),
        button_pages: changed(old.button_pages != new.button_pages).map(|_| new.button_pages.clone()),
        button_scale: changed(old.button_scale != new.button_scale).map(|_| new.button_scale),
        stop_all_releases: changed(old.stop_all_releases != new.stop_all_releases).map(|_| new.stop_all_releases),
        release_fade: changed(old.release_fade != new.release_fade).map(|_| new.release_fade),
    };
    (p != LightingPatch::default()).then_some(p)
}

/// Apply a [`diff`] to the design it was made from. False (design untouched) if it doesn't fit:
/// the copy has drifted, so the whole design has to be sent again.
pub fn apply(l: &mut Lighting, p: &LightingPatch) -> bool {
    let mut n = l.clone();
    let ok = p.fixture_types.as_ref().is_none_or(|x| apply_list(&mut n.fixture_types, x, |t| t.id.clone()))
        && p.fixtures.as_ref().is_none_or(|x| apply_list(&mut n.fixtures, x, |f| f.id))
        && p.groups.as_ref().is_none_or(|x| apply_list(&mut n.groups, x, |g| g.id))
        && p.sequences.as_ref().is_none_or(|x| apply_sequences(&mut n.sequences, x))
        && p.presets.as_ref().is_none_or(|x| apply_list(&mut n.presets, x, |p| p.id))
        && p.points.as_ref().is_none_or(|x| apply_list(&mut n.points, x, |p| p.id))
        && p.effects.as_ref().is_none_or(|x| apply_list(&mut n.effects, x, |e| e.id))
        && p.buttons.as_ref().is_none_or(|x| apply_list(&mut n.buttons, x, |b| b.id))
        && p.shapes.as_ref().is_none_or(|x| apply_list(&mut n.shapes, x, |s| s.id));
    if !ok {
        return false;
    }
    if let Some(o) = &p.outputs {
        n.outputs = o.clone();
    }
    if let Some(i) = &p.inputs {
        n.inputs = i.clone();
    }
    if let Some(v) = &p.plot {
        n.plot = v.clone();
    }
    if let Some(v) = p.stop_all_releases {
        n.stop_all_releases = v;
    }
    if let Some(v) = p.release_fade {
        n.release_fade = v;
    }
    if let Some(v) = p.button_scale {
        n.button_scale = v;
    }
    if let Some(v) = &p.button_pages {
        n.button_pages = v.clone();
    }
    *l = n;
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::light::library;
    use crate::light::model::Attribute;

    fn rig() -> Lighting {
        let mut l = library::bench_rig(40, 3, 20);
        l.groups.push(Group { id: 1, name: "All".into(), fixtures: (1..=40).collect() });
        l.presets.push(Preset { id: 1, name: "Red".into(), ..Default::default() });
        l.effects.push(Effect { id: 1, ..Default::default() });
        l
    }

    fn roundtrip(old: &Lighting, new: &Lighting) -> usize {
        let p = diff(old, new).expect("changed");
        let json = serde_json::to_vec(&p).unwrap();
        let p: LightingPatch = serde_json::from_slice(&json).unwrap();
        let mut copy = old.clone();
        assert!(apply(&mut copy, &p));
        assert_eq!(&copy, new);
        json.len()
    }

    #[test]
    fn small_edits_make_small_patches() {
        let old = rig();
        assert!(diff(&old, &old).is_none());
        let whole = serde_json::to_vec(&old).unwrap().len();
        // One cue value.
        let mut new = old.clone();
        new.sequences[1].cues[7].values.entry(3).or_default().insert(Attribute::Dimmer, 0.25.into());
        let n = roundtrip(&old, &new);
        assert!(n * 20 < whole, "{n} of {whole}");
        // A sequence setting, a fixture moved, a group renamed, an output added.
        let mut new = old.clone();
        new.sequences[0].name = "Main".into();
        new.fixtures[4].position = [1.0, 2.0, 3.0];
        new.groups[0].name = "Everything".into();
        new.outputs.push(Default::default());
        new.release_fade = 3.0;
        new.points.push(FocusPoint { id: 1, name: "Centre".into(), position: [0.0, 0.0, 1.0] });
        roundtrip(&old, &new);
    }

    #[test]
    fn added_removed_and_reordered() {
        let old = rig();
        let mut new = old.clone();
        new.fixtures.remove(2);
        new.fixtures.swap(0, 5);
        new.fixtures.push(Fixture { id: 99, ..Default::default() });
        new.sequences[2].cues.remove(4);
        new.sequences[2].cues.insert(0, LightCue::default());
        new.sequences.swap(0, 1);
        new.sequences.push(Sequence { id: 9, ..Default::default() });
        new.presets.clear();
        roundtrip(&old, &new);
        // Duplicate ids go whole.
        let mut dup = old.clone();
        dup.fixtures[1].id = dup.fixtures[0].id;
        assert!(matches!(diff(&old, &dup).unwrap().fixtures, Some(ListPatch::Whole(_))));
        roundtrip(&old, &dup);
    }

    #[test]
    fn drifted_copy_is_refused() {
        let old = rig();
        let mut new = old.clone();
        new.sequences[0].cues[0].name = "x".into();
        new.fixtures[0].name = "y".into();
        let p = diff(&old, &new).unwrap();
        let mut other = old.clone();
        other.sequences[0].cues.remove(0);
        let before = other.clone();
        assert!(!apply(&mut other, &p));
        assert_eq!(other, before, "untouched");
    }

    /// Every field of a design is covered by the patch (a new field must be added to it).
    #[test]
    fn every_field_is_patched() {
        let mut l = rig();
        l.inputs.push(Default::default());
        l.shapes.push(PlotShape { id: 1, ..Default::default() });
        l.points.push(FocusPoint { id: 1, ..Default::default() });
        let v = serde_json::to_value(&l).unwrap();
        let fields: Vec<&String> = v.as_object().unwrap().keys().collect();
        let p = serde_json::to_value(LightingPatch::default()).unwrap();
        for f in fields {
            assert!(p.get(f).is_some(), "LightingPatch doesn't carry `{f}`");
        }
    }
}
