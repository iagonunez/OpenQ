//! Lighting playback: the compiled show ([`LightShow`]) and the state machine that turns GOs
//! into DMX frames ([`Playback`]). Pure logic, no I/O or threads (see [`super::engine`]).
//!
//! * Every (fixture, attribute) pair is a *slot*; values are normalised `f32`.
//! * A sequence GO builds one fade *track* per slot it touches: from the slot's current value to
//!   the cue's tracked value, after the cue's delay, over its fade (down fade for intensity going
//!   down). Slots the new cue doesn't hold are released (intensity to 0, others home).
//! * Resolve: intensity is highest-takes-precedence across sequences (scaled by each sequence's
//!   level), every other attribute latest-takes-precedence (a slot belongs to the sequence that
//!   last *changed* it). A released LTP slot fades to the value underneath. The programmer
//!   overrides both; the grand master scales intensity last.
//! * A compiled sequence stores the full tracked state of every [`SNAPSHOT`]-th cue and only the
//!   changes of the others; [`LightShow::compile_from`] keeps unchanged sequences of the
//!   previous compile, so a cue edit recompiles one sequence.
//! * `tick` doesn't allocate once the show is loaded; GO only reuses buffers.

use super::dmx::DmxFrame;
use super::effects::{CompiledEffect, Effect, EffectId, Member, Shape};
use super::model::{
    Attribute, ChannelDef, Feature, FixtureId, LightCueId, Lighting, SequenceId, cell_id, MAX_CELLS, MAX_FIXTURE_ID, MAX_UNIVERSE, UNIVERSE_SIZE,
};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

/// Error of a GO past the last cue of a sequence.
pub const END_OF_SEQUENCE: &str = "end of sequence";

/// A compiled sequence keeps the full tracked state of every `SNAPSHOT`-th cue; the cues in
/// between keep only what they change (GO = nearest snapshot + the changes up to the cue).
pub const SNAPSHOT: usize = 32;

/// How many earlier cues' effects can be fading out at once in a sequence (quick GOs); beyond
/// that the oldest stops at once.
const OLD_EFFECTS: usize = 4;

/// One controllable value: an attribute of a fixture.
#[derive(Clone, Debug, PartialEq)]
pub struct SlotInfo {
    /// Fixture.
    pub fixture: FixtureId,
    /// Attribute.
    pub attr: Attribute,
    /// Value with nothing controlling it.
    pub home: f32,
}

#[derive(Clone, Debug)]
struct FixtureOut {
    universe: u16,
    /// 0-based first slot in the universe.
    start: usize,
    channels: Vec<(ChannelDef, u32)>,
    /// Virtual dimmer slot (fixtures without a dimmer channel).
    dimmer: Option<u32>,
    invert_pan: bool,
    invert_tilt: bool,
}

/// One slot of a cue's tracked state. Kept small: a big rig has many of these.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Entry {
    /// Slot.
    pub slot: u32,
    /// Target value (NaN in [`CompiledCue::changes`]: the slot is no longer held).
    pub value: f32,
}

/// When and how fast a value is reached.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Timing {
    /// Delay, seconds.
    pub delay: f64,
    /// Fade, seconds (intensity going up, and every other attribute).
    pub fade: f64,
    /// Fade when intensity goes down.
    pub down: f64,
}

/// A lighting cue ready for playback. Its full tracked state comes from its sequence
/// ([`CompiledSequence::state_into`]).
#[derive(Clone, Debug)]
pub struct CompiledCue {
    /// Design id.
    pub id: LightCueId,
    /// Fade, seconds.
    pub fade: f64,
    /// Intensity down fade, seconds.
    pub down_fade: f64,
    /// Delay, seconds.
    pub delay: f64,
    /// Timing of each feature's attributes ([`Feature::ALL`] order).
    pub timings: [Timing; Feature::ALL.len()],
    /// Slots whose tracked value differs from the previous cue's, sorted by slot (NaN = no
    /// longer held). Empty for a cue with a snapshot.
    pub changes: Vec<Entry>,
    /// Move in black: values of the next cue this one already sets (while dark), sorted by slot.
    pub moves: Vec<(Entry, Timing)>,
    /// Effects running in this cue (started here or tracked from earlier cues).
    pub effects: Vec<CompiledEffect>,
    /// Seconds from GO until every fade of this cue is complete.
    pub duration: f64,
}

impl CompiledCue {
    /// Seconds from GO until every fade of this cue is complete.
    pub fn duration(&self) -> f64 {
        self.duration
    }

    /// The timing of an attribute's values in this cue.
    pub fn timing(&self, attr: Attribute) -> Timing {
        self.timings[attr.feature() as usize]
    }

    /// Timing of an entry of its state (moves in black have their own).
    fn entry_timing(&self, slot: u32, attr: Attribute) -> Timing {
        match self.moves.binary_search_by_key(&slot, |m| m.0.slot) {
            Ok(k) => self.moves[k].1,
            Err(_) => self.timing(attr),
        }
    }
}

/// A sequence ready for playback.
#[derive(Clone, Debug)]
pub struct CompiledSequence {
    /// Design id.
    pub id: SequenceId,
    /// Cues in order.
    pub cues: Vec<CompiledCue>,
    /// Full tracked state of cues 0, [`SNAPSHOT`], 2 × [`SNAPSHOT`]…, sorted by slot.
    snapshots: Vec<Vec<Entry>>,
}

impl CompiledSequence {
    /// Full tracked state of cue `ci` (without its moves in black), sorted by slot, into `out`.
    /// `tmp` is scratch; neither allocates once they have room.
    pub fn state_into(&self, ci: usize, out: &mut Vec<Entry>, tmp: &mut Vec<Entry>) {
        out.clear();
        let Some(snap) = self.snapshots.get(ci / SNAPSHOT) else { return };
        out.extend_from_slice(snap);
        for c in &self.cues[ci / SNAPSHOT * SNAPSHOT + 1..=ci.min(self.cues.len().saturating_sub(1))] {
            apply_changes(out, &c.changes, tmp);
            std::mem::swap(out, tmp);
        }
    }

    /// Full tracked state of cue `ci` (allocates: UI, tests).
    pub fn state(&self, ci: usize) -> Vec<Entry> {
        let (mut out, mut tmp) = (Vec::new(), Vec::new());
        self.state_into(ci, &mut out, &mut tmp);
        out
    }
}

/// `state` with `changes` applied, into `out` (both sorted by slot).
fn apply_changes(state: &[Entry], changes: &[Entry], out: &mut Vec<Entry>) {
    out.clear();
    let (mut i, mut j) = (0, 0);
    while i < state.len() || j < changes.len() {
        let a = state.get(i).map_or(u32::MAX, |e| e.slot);
        let b = changes.get(j).map_or(u32::MAX, |e| e.slot);
        if a < b {
            out.push(state[i]);
            i += 1;
        } else {
            if !changes[j].value.is_nan() {
                out.push(changes[j]);
            }
            i += (a == b) as usize;
            j += 1;
        }
    }
}

/// What changes from `prev` to `cur` (both sorted by slot): new values, and NaN for slots `cur`
/// doesn't hold.
fn diff(prev: &[Entry], cur: &[Entry]) -> Vec<Entry> {
    let mut out = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < prev.len() || j < cur.len() {
        let a = prev.get(i).map_or(u32::MAX, |e| e.slot);
        let b = cur.get(j).map_or(u32::MAX, |e| e.slot);
        if a < b {
            out.push(Entry { slot: a, value: f32::NAN });
            i += 1;
        } else if b < a {
            out.push(cur[j]);
            j += 1;
        } else {
            if prev[i].value != cur[j].value {
                out.push(cur[j]);
            }
            i += 1;
            j += 1;
        }
    }
    out
}

/// The read-only playback form of a [`Lighting`] design.
#[derive(Clone, Debug, Default)]
pub struct LightShow {
    /// Every slot.
    pub slots: Vec<SlotInfo>,
    index: HashMap<(FixtureId, Attribute), u32>,
    fixtures: Vec<FixtureOut>,
    /// Sequences (shared with the previous compile when they didn't change).
    pub sequences: Vec<Arc<CompiledSequence>>,
    /// Patched universes, sorted.
    pub universes: Vec<u16>,
}

impl LightShow {
    /// Compile a design. Fixtures with an unknown type or a duplicate number are skipped;
    /// fixtures outside a valid universe/address get slots but no output.
    pub fn compile(l: &Lighting) -> Self {
        Self::compile_from(l, None)
    }

    /// Compile a design, keeping the compiled sequences of `prev` (the show compiled from the
    /// previous design) that didn't change, when the patch, presets and effects didn't either.
    /// A cue edit then recompiles one sequence instead of the whole design.
    pub fn compile_from(l: &Lighting, prev: Option<(&Lighting, &LightShow)>) -> Self {
        let mut show = LightShow::default();
        let mut seen = HashSet::new();
        let mut valid = Vec::new();
        for f in &l.fixtures {
            let Some(mode) = l.mode_of(f) else { continue };
            if f.id > MAX_FIXTURE_ID || !seen.insert(f.id) {
                continue;
            }
            // Each DMX break is output where it is patched (most modes have one). Checked once per
            // fixture: the footprint walks every channel of the mode, and a pixel bar has many cells.
            valid.clear();
            valid.extend((0..mode.breaks()).filter(|b| {
                let at = f.address_of(*b);
                at.address > 0 && (1..=MAX_UNIVERSE).contains(&at.universe) && at.address as u32 + mode.footprint_of(*b) as u32 - 1 <= UNIVERSE_SIZE as u32
            }));
            // The fixture (master channels), then each cell as a unit of its own.
            let cells = mode.heads.len().min(MAX_CELLS as usize) as u32;
            for cell in std::iter::once(None).chain((1..=cells).map(Some)) {
                let id = cell.map_or(f.id, |c| cell_id(f.id, c));
                let chans = mode.unit_channels(cell).unwrap_or_default();
                for attr in mode.unit_attributes(cell) {
                    let home = chans.iter().find(|c| c.attribute == attr).map_or(attr.home(), |c| c.default_value());
                    show.index.insert((id, attr), show.slots.len() as u32);
                    show.slots.push(SlotInfo { fixture: id, attr, home });
                }
                let has_dimmer = chans.iter().any(|c| c.attribute == Attribute::Dimmer);
                let dimmer = if has_dimmer { None } else { show.index.get(&(id, Attribute::Dimmer)).copied() };
                for &b in &valid {
                    let channels: Vec<(ChannelDef, u32)> = chans.iter().filter(|c| c.dmx_break == b).map(|c| (c.clone(), show.index[&(id, c.attribute)])).collect();
                    if channels.is_empty() {
                        continue;
                    }
                    let at = f.address_of(b);
                    show.fixtures.push(FixtureOut {
                        universe: at.universe,
                        start: at.address as usize - 1,
                        channels,
                        dimmer,
                        invert_pan: f.invert_pan,
                        invert_tilt: f.invert_tilt,
                    });
                }
            }
            for &b in &valid {
                show.universes.push(f.address_of(b).universe);
            }
        }
        show.universes.sort();
        show.universes.dedup();
        // Same patch = same slots, so an unchanged sequence compiles to the same thing.
        let reuse = prev.filter(|(pl, ps)| {
            pl.sequences.len() + pl.buttons.len() == ps.sequences.len() && same_patch(pl, l) && same_aims(pl, l) && pl.presets == l.presets && pl.effects == l.effects
        });
        for s in &l.sequences {
            let kept = reuse.and_then(|(pl, ps)| {
                let i = pl.sequences.iter().position(|x| x.id == s.id)?;
                (pl.sequences[i] == *s).then(|| ps.sequences[i].clone()).filter(|c| c.id == s.id)
            });
            let compiled = kept.unwrap_or_else(|| Arc::new(show.compile_sequence(l, s)));
            show.sequences.push(compiled);
        }
        // Playback buttons: hidden one-cue sequences after the user's.
        for b in &l.buttons {
            let kept = reuse.and_then(|(pl, ps)| {
                let i = pl.buttons.iter().position(|x| x.id == b.id)?;
                (pl.buttons[i] == *b).then(|| ps.sequences[pl.sequences.len() + i].clone()).filter(|c| c.id == b.sequence_id())
            });
            let compiled = kept.unwrap_or_else(|| Arc::new(show.compile_sequence(l, &b.sequence(l))));
            show.sequences.push(compiled);
        }
        show
    }

    /// Whether two designs compile to the same show: only what [`LightShow::compile`] reads is
    /// compared (not fixture names, groups, plot shapes, outputs..., nor positions while there
    /// are no focus points), so edits that don't change playback don't recompile.
    pub fn same_input(a: &Lighting, b: &Lighting) -> bool {
        same_patch(a, b) && same_aims(a, b) && a.sequences == b.sequences && a.presets == b.presets && a.effects == b.effects && a.buttons == b.buttons
    }

    fn compile_sequence(&self, l: &Lighting, s: &super::model::Sequence) -> CompiledSequence {
        let mut cues: Vec<CompiledCue> = Vec::with_capacity(s.cues.len());
        let mut snapshots = Vec::new();
        // Tracked state: slot → value; running effects; full state of the previous cue.
        let mut running: BTreeMap<u32, f32> = BTreeMap::new();
        let mut fx: BTreeMap<EffectId, Vec<FixtureId>> = BTreeMap::new();
        let (mut prev, mut cur): (Vec<Entry>, Vec<Entry>) = (Vec::new(), Vec::new());
        for (ci, c) in s.cues.iter().enumerate() {
            let before = c.cue_only.then(|| (running.clone(), fx.clone()));
            for r in &c.effects {
                if r.stop {
                    fx.remove(&r.effect);
                } else {
                    fx.insert(r.effect, r.fixtures.clone());
                }
            }
            for (fixture, values) in &c.values {
                for (attr, v) in values {
                    if let (Some(&slot), Some(v)) = (self.index.get(&(*fixture, *attr)), l.resolve(*fixture, *attr, *v))
                        && v.is_finite()
                    {
                        running.insert(slot, v.clamp(0.0, 1.0));
                    }
                }
            }
            cur.clear();
            cur.extend(running.iter().map(|(slot, value)| Entry { slot: *slot, value: *value }));
            let changes = if ci % SNAPSHOT == 0 {
                snapshots.push(cur.clone());
                Vec::new()
            } else {
                diff(&prev, &cur)
            };
            let timings = Feature::ALL.map(|f| {
                let (delay, fade, down) = c.feature_timing(f);
                Timing { delay, fade, down }
            });
            let mut cue = CompiledCue {
                id: c.id,
                fade: c.fade.max(0.0),
                down_fade: c.down_fade.unwrap_or(c.fade).max(0.0),
                delay: c.delay.max(0.0),
                timings,
                changes,
                moves: Vec::new(),
                effects: fx.iter().filter_map(|(id, f)| self.compile_effect(l.effect(*id)?, f)).collect(),
                duration: 0.0,
            };
            // Every feature the state holds fades with that feature's timing.
            let mut used = [false; Feature::ALL.len()];
            for e in &cur {
                used[self.slots[e.slot as usize].attr.feature() as usize] = true;
            }
            cue.duration = (0..used.len())
                .filter(|f| used[*f])
                .map(|f| cue.timings[f].delay + cue.timings[f].fade.max(cue.timings[f].down))
                .fold(cue.delay + cue.fade.max(cue.down_fade), f64::max);
            if s.move_in_black
                && let Some(last) = cues.last_mut()
            {
                self.move_in_black(last, &prev, &cur, s.mib_delay.max(0.0), s.mib_fade.max(0.0));
            }
            cues.push(cue);
            if let Some((r, f)) = before {
                running = r;
                fx = f;
            }
            std::mem::swap(&mut prev, &mut cur);
        }
        CompiledSequence { id: s.id, cues, snapshots }
    }

    /// Move in black: fixtures whose intensity ends at 0 in `cue` and comes up in the next cue
    /// get the next cue's other values already, after `cue`'s fades plus `delay`.
    fn move_in_black(&self, cue: &mut CompiledCue, state: &[Entry], next: &[Entry], delay: f64, fade: f64) {
        let at = cue.duration + delay;
        let timing = Timing { delay: at, fade, down: fade };
        let value = |state: &[Entry], slot: u32| state.binary_search_by_key(&slot, |e| e.slot).ok().map(|k| state[k].value);
        for e in next {
            let info = &self.slots[e.slot as usize];
            if info.attr.is_htp() || value(state, e.slot) == Some(e.value) {
                continue;
            }
            let Some(dim) = self.slot(info.fixture, Attribute::Dimmer) else { continue };
            let dark_now = value(state, dim).unwrap_or(0.0) <= 0.0;
            let lit_next = value(next, dim).unwrap_or(0.0) > 0.0;
            if dark_now && lit_next {
                cue.moves.push((*e, timing));
            }
        }
        if !cue.moves.is_empty() {
            cue.duration = cue.duration.max(at + fade);
        }
    }

    /// An effect on fixtures (in phase order), as slots. None if no fixture has the attribute.
    pub fn compile_effect(&self, e: &Effect, fixtures: &[FixtureId]) -> Option<CompiledEffect> {
        if e.shape == Shape::Custom {
            return self.compile_custom(e, fixtures);
        }
        let with: Vec<FixtureId> = fixtures.iter().copied().filter(|f| self.slot(*f, e.attribute).is_some()).collect();
        let members: Vec<Member> = with
            .iter()
            .enumerate()
            .map(|(i, f)| Member {
                slot: self.slot(*f, e.attribute).expect("filtered"),
                slot2: (e.shape == Shape::Circle).then(|| self.slot(*f, Attribute::Tilt)).flatten(),
                phase: e.phase(i, with.len()),
                seed: *f,
                ..Default::default()
            })
            .collect();
        (!members.is_empty()).then(|| CompiledEffect { id: e.id, effect: e.clone(), members })
    }

    /// A custom effect: one member per fixture attribute the table moves (invalid tables don't play).
    fn compile_custom(&self, e: &Effect, fixtures: &[FixtureId]) -> Option<CompiledEffect> {
        let t = e.table.as_ref().filter(|t| t.validate().is_ok())?;
        let with: Vec<FixtureId> = fixtures.iter().copied().filter(|f| t.attributes.iter().any(|a| self.slot(*f, *a).is_some())).collect();
        let mut members = Vec::new();
        for (i, f) in with.iter().enumerate() {
            let phase = e.phase(i, with.len());
            let row = (e.position(i) % t.width as usize) as u32;
            for (col, a) in t.attributes.iter().enumerate() {
                if let Some(slot) = self.slot(*f, *a) {
                    members.push(Member { slot, phase, seed: *f, row, col: col as u32, ..Default::default() });
                }
            }
        }
        (!members.is_empty()).then(|| CompiledEffect { id: e.id, effect: e.clone(), members })
    }

    /// Slot of a fixture attribute.
    pub fn slot(&self, fixture: FixtureId, attr: Attribute) -> Option<u32> {
        self.index.get(&(fixture, attr)).copied()
    }

    /// (sequence index, cue index) of a lighting cue.
    pub fn find_cue(&self, cue: LightCueId) -> Option<(usize, usize)> {
        self.sequences.iter().enumerate().find_map(|(si, s)| s.cues.iter().position(|c| c.id == cue).map(|ci| (si, ci)))
    }
}

/// Same focus points and, when there are any, the same fixture positions and rotations (what
/// [`super::model::Value::Point`] values are worked out from).
fn same_aims(a: &Lighting, b: &Lighting) -> bool {
    a.points == b.points
        && (a.points.is_empty()
            || super::model::Shared::ptr_eq(&a.fixtures, &b.fixtures)
            || a.fixtures.iter().zip(&b.fixtures).all(|(x, y)| x.position == y.position && x.rotation == y.rotation))
}

/// Same fixture types and patch (what slot numbers and outputs come from).
fn same_patch(a: &Lighting, b: &Lighting) -> bool {
    let same = |x: &super::model::Fixture, y: &super::model::Fixture| {
        (x.id, x.universe, x.address, x.invert_pan, x.invert_tilt) == (y.id, y.universe, y.address, y.invert_pan, y.invert_tilt)
            && x.breaks == y.breaks
            && x.fixture_type == y.fixture_type
            && x.mode == y.mode
    };
    a.fixture_types == b.fixture_types && a.fixtures.len() == b.fixtures.len() && a.fixtures.iter().zip(&b.fixtures).all(|(x, y)| same(x, y))
}

#[derive(Clone, Copy, Debug)]
struct Track {
    slot: u32,
    from: f32,
    /// Target (for a released LTP slot: home, but it fades to whatever is underneath).
    to: f32,
    start: f64,
    dur: f64,
    /// LTP order: when this sequence last changed the slot.
    stamp: u64,
    /// Fading out; dropped when done.
    release: bool,
}

impl Track {
    /// How far through its fade, 0..=1.
    fn progress(&self, t: f64) -> f32 {
        if t < self.start {
            0.0
        } else if self.dur <= 0.0 || t >= self.start + self.dur {
            1.0
        } else {
            ((t - self.start) / self.dur) as f32
        }
    }

    fn value(&self, t: f64) -> f32 {
        self.from + (self.to - self.from) * self.progress(t)
    }

    fn done(&self, t: f64) -> bool {
        t >= self.start + self.dur
    }
}

#[derive(Clone, Debug)]
struct SeqState {
    cue: Option<usize>,
    level: f32,
    /// Sorted by slot.
    tracks: Vec<Track>,
    /// Some track is a release (the tick's release pass skips sequences without one).
    releasing: bool,
    /// Effects of this cue index are running, fading in from (start, duration).
    effects: Option<FxState>,
    /// Earlier cues' effects still fading out, oldest first.
    old_effects: [Option<FxState>; OLD_EFFECTS],
}

/// Which cue's effect list runs and its fade.
#[derive(Clone, Copy, Debug)]
struct FxState {
    cue: usize,
    start: f64,
    dur: f64,
}

impl FxState {
    fn ramp(&self, t: f64) -> f32 {
        if self.dur <= 0.0 { (t >= self.start) as u8 as f32 } else { ((t - self.start) / self.dur).clamp(0.0, 1.0) as f32 }
    }
}

impl SeqState {
    /// Start fading out a cue's effects (the oldest of a full list stops at once).
    fn fade_out(&mut self, f: FxState) {
        match self.old_effects.iter_mut().find(|o| o.is_none()) {
            Some(free) => *free = Some(f),
            None => {
                self.old_effects.rotate_left(1);
                self.old_effects[OLD_EFFECTS - 1] = Some(f);
            }
        }
    }
}

impl Default for SeqState {
    fn default() -> Self {
        Self { cue: None, level: 1.0, tracks: Vec::new(), releasing: false, effects: None, old_effects: [None; OLD_EFFECTS] }
    }
}

/// What a sequence is doing, for the UI.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SequenceStatus {
    /// Sequence.
    pub id: SequenceId,
    /// Current cue.
    pub cue: Option<LightCueId>,
    /// Holds or fades any values.
    pub active: bool,
    /// Master level.
    pub level: f32,
}

/// Lighting playback state.
pub struct Playback {
    show: Arc<LightShow>,
    epoch: Instant,
    seqs: Vec<SeqState>,
    stamp: u64,
    /// Resolved values of the last tick (programmer and effects included).
    values: Vec<f32>,
    /// The same from sequences only: where a new fade starts.
    base: Vec<f32>,
    best: Vec<u64>,
    programmer: Vec<Option<f32>>,
    scratch: Vec<Track>,
    /// A cue's full state while it is GOed, and scratch for building it.
    state: Vec<Entry>,
    state_tmp: Vec<Entry>,
    /// Grand master, 0..=1 (scales intensity).
    pub grand_master: f32,
    /// Blackout: intensity 0.
    pub blackout: bool,
    /// Tempo for effects that follow it.
    pub bpm: f32,
    /// Audio envelope for audio-reactive effects (see [`Playback::set_audio`]), and its time.
    audio: (f32, f64),
    /// Effects run from the programmer (on top of everything).
    programmer_effects: Vec<CompiledEffect>,
    frame: DmxFrame,
}

impl Playback {
    /// Playback of a compiled show; time is measured from `epoch`.
    pub fn new(show: Arc<LightShow>, epoch: Instant) -> Self {
        let mut p = Self {
            show: Arc::new(LightShow::default()),
            epoch,
            seqs: Vec::new(),
            stamp: 0,
            values: Vec::new(),
            base: Vec::new(),
            best: Vec::new(),
            programmer: Vec::new(),
            scratch: Vec::new(),
            state: Vec::new(),
            state_tmp: Vec::new(),
            grand_master: 1.0,
            blackout: false,
            bpm: 120.0,
            audio: (0.0, 0.0),
            programmer_effects: Vec::new(),
            frame: DmxFrame::default(),
        };
        p.set_show(show);
        p
    }

    /// The show being played.
    pub fn show(&self) -> &Arc<LightShow> {
        &self.show
    }

    fn time(&self, at: Instant) -> f64 {
        at.saturating_duration_since(self.epoch).as_secs_f64()
    }

    /// Swap in a recompiled show. Running sequences keep their state where the slots and cues
    /// still exist (matched by fixture/attribute and cue id).
    pub fn set_show(&mut self, show: Arc<LightShow>) {
        let old = std::mem::replace(&mut self.show, show);
        let mut seqs = vec![SeqState::default(); self.show.sequences.len()];
        for (os, ostate) in old.sequences.iter().zip(&self.seqs) {
            let Some(ni) = self.show.sequences.iter().position(|s| s.id == os.id) else { continue };
            let n = &mut seqs[ni];
            n.level = ostate.level;
            let remap = |c: usize| os.cues.get(c).and_then(|c| self.show.sequences[ni].cues.iter().position(|x| x.id == c.id));
            let remap_fx = |f: Option<FxState>| f.and_then(|f| remap(f.cue).map(|cue| FxState { cue, ..f }));
            n.cue = ostate.cue.and_then(remap);
            n.effects = remap_fx(ostate.effects);
            n.old_effects = ostate.old_effects.map(remap_fx);
            n.tracks = ostate
                .tracks
                .iter()
                .filter_map(|t| {
                    let s = &old.slots[t.slot as usize];
                    self.show.slot(s.fixture, s.attr).map(|slot| Track { slot, ..*t })
                })
                .collect();
            n.tracks.sort_by_key(|t| t.slot);
            n.releasing = n.tracks.iter().any(|t| t.release);
        }
        self.seqs = seqs;
        let mut programmer = vec![None; self.show.slots.len()];
        for (i, v) in self.programmer.iter().enumerate() {
            if let (Some(v), Some(s)) = (v, old.slots.get(i))
                && let Some(slot) = self.show.slot(s.fixture, s.attr)
            {
                programmer[slot as usize] = Some(*v);
            }
        }
        self.programmer = programmer;
        // Slots moved: the UI sends the programmer's effects again.
        self.programmer_effects.clear();
        let remap_values = |from: &[f32]| {
            let mut values: Vec<f32> = self.show.slots.iter().map(|s| s.home).collect();
            for (i, v) in from.iter().enumerate() {
                if let Some(slot) = old.slots.get(i).and_then(|s| self.show.slot(s.fixture, s.attr)) {
                    values[slot as usize] = *v;
                }
            }
            values
        };
        self.values = remap_values(&self.values);
        self.base = remap_values(&self.base);
        self.best = vec![0; self.show.slots.len()];
        self.frame = DmxFrame::default();
        for u in &self.show.universes {
            self.frame.get_mut(*u);
        }
    }

    /// GO a cue of a sequence (`cue` None = the next one). Returns the cue fired.
    pub fn go(&mut self, seq: SequenceId, cue: Option<LightCueId>, at: Instant) -> Result<LightCueId, String> {
        let si = self.show.sequences.iter().position(|s| s.id == seq).ok_or("unknown sequence")?;
        let show = self.show.clone();
        let sequence = &show.sequences[si];
        let ci = match cue {
            Some(id) => sequence.cues.iter().position(|c| c.id == id).ok_or("unknown lighting cue")?,
            None => self.seqs[si].cue.map_or(0, |c| c + 1),
        };
        let compiled = sequence.cues.get(ci).ok_or(END_OF_SEQUENCE)?;
        sequence.state_into(ci, &mut self.state, &mut self.state_tmp);
        for (e, _) in &compiled.moves {
            match self.state.binary_search_by_key(&e.slot, |x| x.slot) {
                Ok(k) => self.state[k].value = e.value,
                Err(k) => self.state.insert(k, *e),
            }
        }
        let t = self.time(at);
        let st = &mut self.seqs[si];
        std::mem::swap(&mut st.tracks, &mut self.scratch);
        st.tracks.clear();
        st.releasing = false;
        let (old, new) = (&self.scratch, &self.state);
        let (mut i, mut j) = (0, 0);
        while i < old.len() || j < new.len() {
            let os = old.get(i).map_or(u32::MAX, |t| t.slot);
            let ns = new.get(j).map_or(u32::MAX, |n| n.slot);
            let slot = os.min(ns);
            let o = (os == slot).then(|| old[i]);
            let n = (ns == slot).then(|| new[j]);
            i += o.is_some() as usize;
            j += n.is_some() as usize;
            let info = &show.slots[slot as usize];
            let htp = info.attr.is_htp();
            // From what the sequences put on stage (not the programmer or effects on top).
            let from = match o {
                Some(o) if o.release && !htp => self.base[slot as usize],
                Some(o) => o.value(t),
                None if htp => 0.0,
                None => self.base[slot as usize],
            };
            let (to, release) = match n {
                Some(e) => (e.value, false),
                None => (if htp { 0.0 } else { info.home }, true),
            };
            // Same target as the running track: let its fade carry on.
            if let Some(o) = o
                && !o.release
                && !release
                && o.to == to
            {
                st.tracks.push(o);
                continue;
            }
            let changed = match o {
                Some(o) => o.release || o.to != to,
                None => true,
            };
            let stamp = if changed && !release {
                self.stamp += 1;
                self.stamp
            } else {
                o.map_or(0, |o| o.stamp)
            };
            let timing = match n {
                Some(_) => compiled.entry_timing(slot, info.attr),
                None => Timing { delay: compiled.delay, fade: compiled.fade, down: compiled.down_fade },
            };
            let dur = if htp && to < from { timing.down } else { timing.fade };
            st.releasing |= release;
            st.tracks.push(Track { slot, from, to, start: t + timing.delay, dur, stamp, release });
        }
        if let Some(f) = st.effects {
            st.fade_out(FxState { start: t + compiled.delay, dur: compiled.fade, ..f });
        }
        st.effects = Some(FxState { cue: ci, start: t + compiled.delay, dur: compiled.fade });
        st.cue = Some(ci);
        Ok(compiled.id)
    }

    /// Fade a sequence out over `fade` seconds and forget its cue.
    pub fn release(&mut self, seq: SequenceId, fade: f64, at: Instant) {
        let t = self.time(at);
        let Some(si) = self.show.sequences.iter().position(|s| s.id == seq) else { return };
        let show = self.show.clone();
        let st = &mut self.seqs[si];
        for tr in &mut st.tracks {
            let info = &show.slots[tr.slot as usize];
            let htp = info.attr.is_htp();
            *tr = Track {
                from: if !htp && tr.release { self.base[tr.slot as usize] } else { tr.value(t) },
                to: if htp { 0.0 } else { info.home },
                start: t,
                dur: fade.max(0.0),
                release: true,
                ..*tr
            };
        }
        st.releasing = !st.tracks.is_empty();
        if let Some(f) = st.effects.take() {
            st.fade_out(FxState { start: t, dur: fade.max(0.0), ..f });
        }
        st.cue = None;
    }

    /// Release every sequence.
    pub fn release_all(&mut self, fade: f64, at: Instant) {
        let ids: Vec<SequenceId> = self.show.sequences.iter().map(|s| s.id).collect();
        for id in ids {
            self.release(id, fade, at);
        }
    }

    /// Set a sequence's master level (scales its intensity).
    pub fn set_level(&mut self, seq: SequenceId, level: f32) {
        if let Some(si) = self.show.sequences.iter().position(|s| s.id == seq) {
            self.seqs[si].level = level.clamp(0.0, 1.0);
        }
    }

    /// Replace the programmer (values that override playback).
    pub fn set_programmer(&mut self, values: &[(FixtureId, Attribute, f32)]) {
        self.programmer.iter_mut().for_each(|v| *v = None);
        for (f, a, v) in values {
            if let Some(slot) = self.show.slot(*f, *a) {
                self.programmer[slot as usize] = Some(v.clamp(0.0, 1.0));
            }
        }
    }

    /// Feed the audio peak measured since the last call (linear 0..1): the envelope jumps up to
    /// it and falls with [`super::effects::AUDIO_RELEASE`].
    pub fn set_audio(&mut self, peak: f32, now: Instant) {
        let t = self.time(now);
        let dt = (t - self.audio.1).max(0.0) as f32;
        let fallen = self.audio.0 * (-dt / super::effects::AUDIO_RELEASE).exp();
        self.audio = (if peak.is_finite() { fallen.max(peak.clamp(0.0, 1.0)) } else { fallen }, t);
    }

    /// The audio envelope effects follow.
    pub fn audio_level(&self) -> f32 {
        self.audio.0
    }

    /// Replace the programmer's effects.
    pub fn set_programmer_effects(&mut self, effects: &[(Effect, Vec<FixtureId>)]) {
        self.programmer_effects = effects.iter().filter_map(|(e, f)| self.show.compile_effect(e, f)).collect();
    }

    /// Sequence states, for the UI.
    pub fn status(&self) -> Vec<SequenceStatus> {
        let mut out = Vec::new();
        self.status_into(&mut out);
        out
    }

    /// [`Playback::status`] into an existing list: no allocation once it has the capacity
    /// (the light thread refreshes the shared status this way).
    pub fn status_into(&self, out: &mut Vec<SequenceStatus>) {
        out.clear();
        out.extend(self.show.sequences.iter().zip(&self.seqs).map(|(s, st)| SequenceStatus {
            id: s.id,
            cue: st.cue.map(|c| s.cues[c].id),
            active: !st.tracks.is_empty() || st.effects.is_some(),
            level: st.level,
        }));
    }

    /// Resolved value of a slot after the last tick (before the grand master).
    pub fn value(&self, slot: u32) -> Option<f32> {
        self.values.get(slot as usize).copied()
    }

    /// Advance to `now` and build the DMX frame. Never inlined, so benchmarks measure the call
    /// the light thread makes.
    #[inline(never)]
    pub fn tick(&mut self, now: Instant) -> &DmxFrame {
        let t = self.time(now);
        let show = &*self.show;
        for (v, s) in self.values.iter_mut().zip(&show.slots) {
            *v = if s.attr.is_htp() { 0.0 } else { s.home };
        }
        self.best.fill(0);
        for st in &self.seqs {
            for tr in &st.tracks {
                let i = tr.slot as usize;
                if show.slots[i].attr.is_htp() {
                    self.values[i] = self.values[i].max(tr.value(t) * st.level);
                } else if !tr.release && tr.stamp >= self.best[i] {
                    self.values[i] = tr.value(t);
                    self.best[i] = tr.stamp.max(1);
                }
            }
        }
        // A released LTP slot fades from where it was to what is underneath: the value of a
        // sequence that changed it earlier, or home (no jump when the release ends).
        for st in self.seqs.iter_mut().filter(|st| st.releasing) {
            for tr in &st.tracks {
                let i = tr.slot as usize;
                if tr.release && !show.slots[i].attr.is_htp() && tr.stamp >= self.best[i] {
                    self.values[i] = tr.from + (self.values[i] - tr.from) * tr.progress(t);
                    self.best[i] = tr.stamp.max(1);
                }
            }
            st.tracks.retain(|tr| !(tr.release && tr.done(t)));
            st.releasing = st.tracks.iter().any(|tr| tr.release);
        }
        self.base.copy_from_slice(&self.values);
        // Effects of each sequence: the current cue's fade in (unless the previous cue ran them
        // too), earlier cues' fade out (an effect a later list has too is left to that one).
        let htp = |slot: u32| show.slots[slot as usize].attr.is_htp();
        // The envelope as of this frame (it falls between peaks even without a new feed).
        let audio = self.audio.0 * (-((t - self.audio.1).max(0.0) as f32) / super::effects::AUDIO_RELEASE).exp();
        for (si, st) in self.seqs.iter_mut().enumerate() {
            let cues = &show.sequences[si].cues;
            let list = |f: Option<FxState>| f.map_or(&[][..], |f| &cues[f.cue].effects[..]);
            let new = list(st.effects);
            let olds = st.old_effects;
            let prev = list(olds.iter().rev().flatten().next().copied());
            for e in new {
                let carried = prev.iter().any(|o| o.id == e.id);
                let ramp = if carried { 1.0 } else { st.effects.map_or(1.0, |f| f.ramp(t)) };
                e.apply(t, self.bpm, audio, ramp * st.level, &mut self.values, htp);
            }
            for (k, of) in olds.iter().enumerate() {
                let Some(of) = *of else { continue };
                let ramp = of.ramp(t);
                let later = |id| olds[k + 1..].iter().flatten().any(|l| list(Some(*l)).iter().any(|x| x.id == id));
                for e in list(Some(of)).iter().filter(|o| !new.iter().any(|n| n.id == o.id) && !later(o.id)) {
                    e.apply(t, self.bpm, audio, (1.0 - ramp) * st.level, &mut self.values, htp);
                }
                if ramp >= 1.0 {
                    st.old_effects[k] = None;
                }
            }
            // Keep the fading lists oldest first, without gaps.
            let mut n = 0;
            for k in 0..OLD_EFFECTS {
                if let Some(f) = st.old_effects[k].take() {
                    st.old_effects[n] = Some(f);
                    n += 1;
                }
            }
        }
        for (v, p) in self.values.iter_mut().zip(&self.programmer) {
            if let Some(p) = p {
                *v = *p;
            }
        }
        for e in &self.programmer_effects {
            e.apply(t, self.bpm, audio, 1.0, &mut self.values, htp);
        }
        let master = if self.blackout { 0.0 } else { self.grand_master.clamp(0.0, 1.0) };
        self.frame.clear();
        for fo in &show.fixtures {
            let u = self.frame.get_mut(fo.universe);
            let dim = fo.dimmer.map_or(1.0, |d| self.values[d as usize]);
            for (c, slot) in &fo.channels {
                let mut v = self.values[*slot as usize];
                match c.attribute {
                    Attribute::Dimmer => v *= master,
                    a if a.is_emitter() && fo.dimmer.is_some() => v *= dim * master,
                    Attribute::Pan if fo.invert_pan => v = 1.0 - v,
                    Attribute::Tilt if fo.invert_tilt => v = 1.0 - v,
                    _ => {}
                }
                c.write(v, fo.start, &mut u.data);
            }
        }
        &self.frame
    }

    /// The last frame built.
    pub fn frame(&self) -> &DmxFrame {
        &self.frame
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::light::library;
    use crate::light::model::{Fixture, LightCue, Sequence};
    use std::time::Duration;

    /// Dimmer 1 at 1.1, RGB 2 at 1.2..4, spot 3 at 1.10.
    fn design() -> Lighting {
        let mut l = Lighting::default();
        for (id, ty, addr) in [(1, "generic/dimmer", 1), (2, "generic/rgb", 2), (3, "generic/spot", 10)] {
            let t = library::generic(ty).unwrap();
            l.add_type(&t);
            l.fixtures.push(Fixture { id, fixture_type: t.id.clone(), mode: t.modes[0].name.clone(), address: addr, ..Default::default() });
        }
        let cue = |vals: &[(FixtureId, Attribute, f32)], fade: f64| {
            let mut c = LightCue { fade, ..Default::default() };
            for (f, a, v) in vals {
                c.values.entry(*f).or_default().insert(*a, (*v).into());
            }
            c
        };
        l.sequences.push(Sequence {
            id: 1,
            name: "Main".into(),
            cues: vec![
                cue(&[(1, Attribute::Dimmer, 1.0), (2, Attribute::Dimmer, 1.0), (2, Attribute::Red, 1.0)], 2.0),
                cue(&[(1, Attribute::Dimmer, 0.0), (3, Attribute::Pan, 1.0), (3, Attribute::Dimmer, 0.5)], 0.0),
            ].into(),
            ..Default::default()
        });
        l.sequences.push(Sequence { id: 2, name: "Sub".into(), cues: vec![cue(&[(1, Attribute::Dimmer, 0.4), (3, Attribute::Pan, 0.25)], 0.0)].into(), ..Default::default() });
        l
    }

    fn at(t0: Instant, s: f64) -> Instant {
        t0 + Duration::from_secs_f64(s)
    }

    #[test]
    fn fade_tracking_and_virtual_dimmer() {
        let t0 = Instant::now();
        let mut p = Playback::new(Arc::new(LightShow::compile(&design())), t0);
        p.go(1, None, t0).unwrap();
        let f = p.tick(at(t0, 1.0)).get(1).unwrap().data;
        assert_eq!(f[0], 128, "dimmer halfway through a 2 s fade");
        assert_eq!(f[1], 64, "red 0.5 × virtual dimmer 0.5");
        let f = p.tick(at(t0, 3.0)).get(1).unwrap().data;
        assert_eq!((f[0], f[1], f[2]), (255, 255, 0));
        // Cue 2 (snap): dimmer 1 out, RGB tracks at full, spot pan to 1.
        p.go(1, None, at(t0, 3.0)).unwrap();
        let f = p.tick(at(t0, 3.0)).get(1).unwrap().data;
        assert_eq!((f[0], f[1]), (0, 255));
        assert_eq!((f[9], f[10]), (255, 255), "pan 16-bit full");
        assert!(p.go(1, None, at(t0, 4.0)).is_err(), "end of sequence");
    }

    /// A 3-cell bar (master dimmer + RGB per cell) at 1.20 plays its cells as units.
    #[test]
    fn multi_cell_fixture() {
        use crate::light::model::{cell_id, ChannelDef, Head};
        let mut l = design();
        let mut bar = library::generic("generic/dimmer").unwrap();
        bar.id = "test/bar".into();
        let m = &mut bar.modes[0];
        m.heads = (0..3u16)
            .map(|i| Head {
                name: (i + 1).to_string(),
                channels: [Attribute::Red, Attribute::Green, Attribute::Blue]
                    .iter()
                    .enumerate()
                    .map(|(k, a)| ChannelDef { attribute: *a, offset: 1 + 3 * i + k as u16, ..Default::default() })
                    .collect(),
            })
            .collect();
        l.add_type(&bar);
        l.fixtures.push(Fixture { id: 20, fixture_type: bar.id.clone(), mode: bar.modes[0].name.clone(), address: 20, ..Default::default() });
        assert_eq!(l.units().iter().filter(|u| crate::light::model::split_id(**u).0 == 20).count(), 4);
        assert_eq!(l.unit_attributes(cell_id(20, 2)), [Attribute::Red, Attribute::Green, Attribute::Blue], "no virtual dimmer under a master dimmer");
        assert!(l.unit(cell_id(20, 4)).is_none(), "no cell 4");
        let show = LightShow::compile(&l);
        assert!(show.slot(cell_id(20, 3), Attribute::Blue).is_some());
        let t0 = Instant::now();
        let mut p = Playback::new(Arc::new(show), t0);
        p.set_programmer(&[(20, Attribute::Dimmer, 1.0), (cell_id(20, 2), Attribute::Green, 1.0), (cell_id(20, 3), Attribute::Red, 1.0)]);
        let f = p.tick(t0).get(1).unwrap().data;
        assert_eq!(&f[19..29], &[255, 0, 0, 0, 0, 255, 0, 255, 0, 0], "master at 20, cell 2 green at 25, cell 3 red at 27");
        // Without a master dimmer each cell gets a virtual one.
        let mut pixels = bar.clone();
        pixels.id = "test/pixels".into();
        pixels.modes[0].channels.clear();
        l.add_type(&pixels);
        l.fixtures.push(Fixture { id: 21, fixture_type: pixels.id.clone(), mode: pixels.modes[0].name.clone(), address: 40, ..Default::default() });
        assert_eq!(l.unit_attributes(cell_id(21, 2)), [Attribute::Dimmer, Attribute::Red, Attribute::Green, Attribute::Blue], "virtual dimmer per cell");
        let mut p = Playback::new(Arc::new(LightShow::compile(&l)), t0);
        p.set_programmer(&[(cell_id(21, 1), Attribute::Dimmer, 1.0), (cell_id(21, 1), Attribute::Red, 1.0), (cell_id(21, 2), Attribute::Red, 1.0)]);
        let f = p.tick(t0).get(1).unwrap().data;
        assert_eq!(&f[40..46], &[255, 0, 0, 0, 0, 0], "cell 1 red; cell 2 dark (its dimmer is 0)");
        // Renumbering the bar moves its cells' values; removing it drops them.
        let mut c = LightCue::default();
        c.values.entry(cell_id(20, 2)).or_default().insert(Attribute::Red, 1.0.into());
        l.sequences[0].cues.push(c);
        assert!(l.renumber_fixture(20, 30));
        assert!(l.sequences[0].cues.last().unwrap().values.contains_key(&cell_id(30, 2)));
        l.remove_fixtures(&[30].into_iter().collect());
        assert!(l.sequences[0].cues.last().unwrap().values.is_empty());
    }

    #[test]
    fn htp_intensity_ltp_position() {
        let t0 = Instant::now();
        let mut p = Playback::new(Arc::new(LightShow::compile(&design())), t0);
        let ids: Vec<_> = p.show().sequences[0].cues.iter().map(|c| c.id).collect();
        p.go(1, Some(ids[1]), t0).unwrap(); // dimmer 1 at 0 (tracked from nothing), pan 1
        p.go(2, None, at(t0, 0.1)).unwrap(); // dimmer 1 at 0.4, pan 0.25
        let f = p.tick(at(t0, 0.2)).get(1).unwrap().data;
        assert_eq!(f[0], 102, "HTP: 0.4 from Sub");
        assert_eq!(f[9], 64, "LTP: Sub changed pan last");
        // Main changes pan again: it takes it back.
        p.go(1, Some(ids[1]), at(t0, 0.3)).unwrap();
        let f = p.tick(at(t0, 0.4)).get(1).unwrap().data;
        assert_eq!(f[9], 64, "same value as before: not a change, Sub keeps pan");
        p.set_level(2, 0.5);
        assert_eq!(p.tick(at(t0, 0.4)).get(1).unwrap().data[0], 51, "sequence level scales HTP");
        p.grand_master = 0.5;
        assert_eq!(p.tick(at(t0, 0.4)).get(1).unwrap().data[0], 26);
        p.blackout = true;
        assert_eq!(p.tick(at(t0, 0.4)).get(1).unwrap().data[0], 0);
    }

    #[test]
    fn release_and_programmer() {
        let t0 = Instant::now();
        let mut p = Playback::new(Arc::new(LightShow::compile(&design())), t0);
        p.go(1, None, t0).unwrap();
        p.tick(at(t0, 5.0));
        p.release(1, 2.0, at(t0, 5.0));
        assert_eq!(p.tick(at(t0, 6.0)).get(1).unwrap().data[0], 128);
        p.tick(at(t0, 7.5));
        assert!(!p.status()[0].active, "released tracks are dropped");
        p.set_programmer(&[(1, Attribute::Dimmer, 0.2)]);
        assert_eq!(p.tick(at(t0, 8.0)).get(1).unwrap().data[0], 51);
    }

    #[test]
    fn recompile_keeps_running_state() {
        let t0 = Instant::now();
        let mut l = design();
        let mut p = Playback::new(Arc::new(LightShow::compile(&l)), t0);
        p.go(1, None, t0).unwrap();
        p.tick(at(t0, 3.0));
        // Insert a fixture before the others: every slot index moves.
        let t = library::generic("generic/dimmer").unwrap();
        l.fixtures.insert(0, Fixture { id: 9, fixture_type: t.id.clone(), mode: t.modes[0].name.clone(), address: 100, ..Default::default() });
        p.set_show(Arc::new(LightShow::compile(&l)));
        let f = p.tick(at(t0, 3.0)).get(1).unwrap().data;
        assert_eq!((f[0], f[1]), (255, 255));
        assert_eq!(p.status()[0].cue, Some(l.sequences[0].cues[0].id));
    }

    #[test]
    fn feature_timing_and_move_in_black() {
        use crate::light::model::{Feature, FeatureTime};
        let t0 = Instant::now();
        let mut l = design();
        // Cue 1 snaps; the spot's pan in cue 2 has its own 4 s fade.
        l.sequences[0].cues[0].fade = 0.0;
        l.sequences[0].cues[1].feature_times.insert(Feature::Position, FeatureTime { fade: 4.0, delay: 0.0 });
        let mut p = Playback::new(Arc::new(LightShow::compile(&l)), t0);
        p.go(1, None, t0).unwrap();
        p.go(1, None, at(t0, 1.0)).unwrap();
        assert_eq!(p.tick(at(t0, 3.0)).get(1).unwrap().data[9], 191, "pan halfway (home 0.5 → 1) through its own fade");
        assert_eq!(p.show().sequences[0].cues[1].duration(), 4.0);
        // Move in black: the spot is dark in cue 1 and lit in cue 2, so cue 1 pre-sets its pan.
        l.sequences[0].move_in_black = true;
        l.sequences[0].mib_delay = 1.0;
        let show = LightShow::compile(&l);
        let pan = show.slot(3, Attribute::Pan).unwrap();
        let cue = &show.sequences[0].cues[0];
        let (e, timing) = cue.moves.iter().find(|m| m.0.slot == pan).expect("moved in cue 1");
        assert_eq!((e.value, timing.delay), (1.0, 1.0));
        let mut p = Playback::new(Arc::new(show), t0);
        p.go(1, None, t0).unwrap();
        assert_eq!(p.tick(at(t0, 3.5)).get(1).unwrap().data[9], 255, "pan moved while dark (1 s delay + 2 s move)");
        assert_eq!(p.frame().get(1).unwrap().data[12], 0, "spot still dark");
    }

    #[test]
    fn multi_break_fixtures_output_each_break_at_its_address() {
        use crate::light::model::{ChannelDef, DmxAddress, FixtureMode, FixtureType};
        let ch = |attribute, dmx_break, offset| ChannelDef { attribute, dmx_break, offset, ..Default::default() };
        let ty = FixtureType {
            id: "test/two".into(),
            modes: vec![FixtureMode { name: "m".into(), channels: vec![ch(Attribute::Dimmer, 0, 0), ch(Attribute::Red, 1, 0), ch(Attribute::Blue, 1, 2)], ..Default::default() }],
            ..Default::default()
        };
        let mut l = Lighting::default();
        l.add_type(&ty);
        let breaks = vec![DmxAddress { universe: 2, address: 10 }];
        l.fixtures.push(Fixture { id: 1, fixture_type: ty.id.clone(), mode: "m".into(), address: 5, breaks, ..Default::default() });
        let t0 = Instant::now();
        let mut p = Playback::new(Arc::new(LightShow::compile(&l)), t0);
        p.set_programmer(&[(1, Attribute::Dimmer, 1.0), (1, Attribute::Red, 1.0), (1, Attribute::Blue, 0.5)]);
        let frame = p.tick(t0);
        assert_eq!(frame.get(1).unwrap().data[4], 255, "break 1 at 1.5");
        assert_eq!(&frame.get(2).unwrap().data[9..12], &[255, 0, 128], "break 2 at 2.10");
        assert_eq!(l.spans(&l.fixtures[0]), [(1, 5, 5), (2, 10, 12)]);
        assert_eq!(l.next_free_address(2, 3), Some(1));
        assert_eq!(l.next_free_address(2, 10), Some(13), "break 2 takes 2.10–12");
        // An unpatched second break: the first still outputs.
        l.fixtures[0].breaks.clear();
        let mut p = Playback::new(Arc::new(LightShow::compile(&l)), t0);
        p.set_programmer(&[(1, Attribute::Dimmer, 1.0)]);
        let frame = p.tick(t0);
        assert_eq!(frame.get(1).unwrap().data[4], 255);
        assert!(frame.get(2).is_none());
    }

    #[test]
    fn audio_envelope_rises_at_once_and_falls_slowly() {
        let show = Arc::new(LightShow::compile(&design()));
        let t0 = Instant::now();
        let mut p = Playback::new(show, t0);
        p.set_audio(0.8, t0);
        assert_eq!(p.audio_level(), 0.8);
        p.set_audio(0.1, t0 + Duration::from_secs_f32(super::super::effects::AUDIO_RELEASE));
        assert!((p.audio_level() - 0.8 / std::f32::consts::E).abs() < 1e-3, "{}", p.audio_level());
        p.set_audio(f32::NAN, t0 + Duration::from_secs(5));
        assert!(p.audio_level() < 0.01);
    }

    #[test]
    fn effects_track_fade_and_stop() {
        use crate::light::effects::{Effect, EffectRun, Shape};
        let t0 = Instant::now();
        let mut l = design();
        // A square chase on the dimmer of fixture 1 (absolute, 1 Hz), started in cue 1, stopped
        // in a new cue 3; cue 2 doesn't mention it, so it keeps running.
        l.effects.push(Effect { id: 1, shape: Shape::Square, speed: 1.0, spread: 0.0, ..Default::default() });
        l.sequences[0].cues[0].fade = 0.0;
        l.sequences[0].cues[0].effects.push(EffectRun { effect: 1, fixtures: vec![1], stop: false });
        l.sequences[0].cues.push(LightCue { fade: 0.0, effects: vec![EffectRun { effect: 1, stop: true, ..Default::default() }], ..Default::default() });
        let mut p = Playback::new(Arc::new(LightShow::compile(&l)), t0);
        p.go(1, None, t0).unwrap();
        assert_eq!(p.tick(at(t0, 10.25)).get(1).unwrap().data[0], 255, "on half");
        assert_eq!(p.tick(at(t0, 10.75)).get(1).unwrap().data[0], 0, "off half");
        p.go(1, None, at(t0, 11.0)).unwrap();
        assert_eq!(p.tick(at(t0, 11.75)).get(1).unwrap().data[0], 0, "still running in cue 2");
        p.go(1, None, at(t0, 12.0)).unwrap();
        // Cue 2 set fixture 1 to 0 and cue 3 tracks that: with the effect gone it stays at 0.
        assert_eq!(p.tick(at(t0, 12.25)).get(1).unwrap().data[0], 0, "stopped");
        assert!(p.show().sequences[0].cues[1].effects.len() == 1 && p.show().sequences[0].cues[2].effects.is_empty());
        // Programmer effect with tempo sync: 1 cycle per beat at 60 BPM.
        p.bpm = 60.0;
        p.set_programmer_effects(&[(Effect { shape: Shape::SawUp, sync: true, speed: 1.0, spread: 0.0, ..Default::default() }, vec![1])]);
        assert_eq!(p.tick(at(t0, 20.5)).get(1).unwrap().data[0], 128);
    }

    #[test]
    fn cues_follow_preset_edits() {
        use crate::light::model::{Preset, Value};
        let t0 = Instant::now();
        let mut l = design();
        l.presets.push(Preset { id: 1, values: [(2, [(Attribute::Red, 0.2)].into_iter().collect())].into_iter().collect::<BTreeMap<_, _>>().into(), ..Default::default() });
        l.sequences[0].cues[0].values.get_mut(&2).unwrap().insert(Attribute::Red, Value::Preset { preset: 1 });
        let mut p = Playback::new(Arc::new(LightShow::compile(&l)), t0);
        p.go(1, None, t0).unwrap();
        assert_eq!(p.tick(at(t0, 5.0)).get(1).unwrap().data[1], 51);
        l.presets[0].values.get_mut(&2).unwrap().insert(Attribute::Red, 1.0);
        p.set_show(Arc::new(LightShow::compile(&l)));
        p.go(1, Some(l.sequences[0].cues[0].id), at(t0, 5.0)).unwrap();
        assert_eq!(p.tick(at(t0, 10.0)).get(1).unwrap().data[1], 255, "the cue follows the preset");
    }

    /// A blackout cue (`Lighting::blackout_look`) takes every intensity, virtual dimmers
    /// included, to zero and leaves the other attributes where they are.
    #[test]
    fn blackout_cue() {
        let t0 = Instant::now();
        let mut l = design();
        let look = l.blackout_look();
        assert_eq!(look.len(), 3, "dimmer, RGB (virtual dimmer) and spot");
        l.sequences[0].cues.push(LightCue { fade: 0.0, values: look.into(), ..Default::default() });
        let ids: Vec<_> = l.sequences[0].cues.iter().map(|c| c.id).collect();
        let mut p = Playback::new(Arc::new(LightShow::compile(&l)), t0);
        p.go(1, Some(ids[1]), t0).unwrap();
        let f = p.tick(at(t0, 1.0)).get(1).unwrap().data;
        assert!(f[1] > 0 && f[9] == 255, "RGB red on, spot pan at full");
        p.go(1, Some(ids[2]), at(t0, 1.0)).unwrap();
        let f = p.tick(at(t0, 1.1)).get(1).unwrap().data;
        assert_eq!((f[0], f[1]), (0, 0), "dimmer out, red scaled to 0 by the virtual dimmer");
        assert_eq!(f[9], 255, "pan tracks");
    }

    /// A playback button: a preset on its fixtures plus its own values, played as a hidden
    /// sequence; it stays on across a recompile and survives preset deletion as plain values.
    #[test]
    fn buttons_play_as_sequences() {
        use crate::light::model::{LightButton, Preset};
        let t0 = Instant::now();
        let mut l = design();
        l.presets.push(Preset { id: 1, values: [(2, [(Attribute::Red, 0.2)].into_iter().collect())].into_iter().collect::<BTreeMap<_, _>>().into(), ..Default::default() });
        let mut b = LightButton { id: 3, preset: Some(1), fixtures: vec![2], ..Default::default() };
        b.values.entry(1).or_default().insert(Attribute::Dimmer, 0.8.into());
        b.values.entry(2).or_default().insert(Attribute::Dimmer, 1.0.into());
        l.buttons.push(b.clone());
        let show = LightShow::compile(&l);
        assert_eq!(show.sequences.len(), 3);
        assert_eq!(show.sequences[2].id, b.sequence_id());
        let mut p = Playback::new(Arc::new(show), t0);
        p.go(b.sequence_id(), Some(b.cue_id()), t0).unwrap();
        let f = p.tick(at(t0, 0.1)).get(1).unwrap().data;
        assert_eq!((f[0], f[1]), (204, 51), "own value on 1, preset red on 2");
        // An edit recompiles; the button stays on.
        l.buttons[0].name = "Fog".into();
        let next = LightShow::compile_from(&l, Some((&design(), p.show())));
        p.set_show(Arc::new(next));
        assert_eq!(p.tick(at(t0, 0.2)).get(1).unwrap().data[0], 204);
        assert!(p.status().iter().any(|s| s.id == b.sequence_id() && s.cue == Some(b.cue_id())));
        p.release(b.sequence_id(), 0.0, at(t0, 0.3));
        assert_eq!(p.tick(at(t0, 0.4)).get(1).unwrap().data[0], 0);
        // Deleting the preset keeps its level in the button.
        l.delete_preset(1);
        assert_eq!(l.buttons[0].preset, None);
        assert_eq!(l.buttons[0].values[&2][&Attribute::Red], 0.2.into());
        // Renumbering and removing fixtures follow into buttons.
        assert!(l.renumber_fixture(2, 20));
        assert!(l.buttons[0].values.contains_key(&20));
        l.remove_fixtures(&[1].into_iter().collect());
        assert!(!l.buttons[0].values.contains_key(&1));
    }

    /// Releasing the sequence that owns an LTP slot fades it to the value an older sequence
    /// holds there, with no jump when the release ends.
    #[test]
    fn ltp_release_fades_to_the_value_underneath() {
        let t0 = Instant::now();
        let mut p = Playback::new(Arc::new(LightShow::compile(&design())), t0);
        let ids: Vec<_> = p.show().sequences[0].cues.iter().map(|c| c.id).collect();
        p.go(2, None, t0).unwrap(); // pan 0.25
        p.go(1, Some(ids[1]), at(t0, 0.1)).unwrap(); // pan 1, changed later: Main owns it
        assert_eq!(p.tick(at(t0, 0.5)).get(1).unwrap().data[9], 255);
        p.release(1, 2.0, at(t0, 1.0));
        assert_eq!(p.tick(at(t0, 2.0)).get(1).unwrap().data[9], 159, "halfway from 1 to Sub's 0.25");
        assert_eq!(p.tick(at(t0, 3.5)).get(1).unwrap().data[9], 64);
        assert_eq!(p.tick(at(t0, 4.0)).get(1).unwrap().data[9], 64, "no jump after the release");
    }

    /// A GO starts its fades from what the sequences put on stage, not from the programmer.
    #[test]
    fn fades_start_from_playback_not_the_programmer() {
        let t0 = Instant::now();
        let mut l = design();
        l.sequences[1].cues[0].fade = 2.0;
        let mut p = Playback::new(Arc::new(LightShow::compile(&l)), t0);
        p.set_programmer(&[(3, Attribute::Pan, 1.0)]);
        p.tick(t0);
        p.go(2, None, t0).unwrap(); // pan from home 0.5 to 0.25 over 2 s
        p.set_programmer(&[]);
        assert_eq!(p.tick(at(t0, 1.0)).get(1).unwrap().data[9], 96, "0.375, not halfway from the programmer's 1");
    }

    /// Cue states rebuilt from snapshots and changes equal the tracked state, across snapshots
    /// and with cue-only cues.
    #[test]
    fn snapshots_and_changes_rebuild_the_tracked_state() {
        let mut l = design();
        let n = SNAPSHOT * 2 + 5;
        l.sequences[0].cues = (0..n)
            .map(|k| {
                let mut c = LightCue { fade: 0.0, cue_only: k % 7 == 3, ..Default::default() };
                let v = c.values.entry(1 + (k as u32 % 3)).or_default();
                v.insert(Attribute::Dimmer, (k as f32 / n as f32).into());
                if k == 11 {
                    c.values.entry(3).or_default().insert(Attribute::Pan, 0.75.into());
                }
                c
            })
            .collect();
        let show = LightShow::compile(&l);
        let seq = &show.sequences[0];
        // Naive tracking.
        let mut running: BTreeMap<u32, f32> = BTreeMap::new();
        for (k, c) in l.sequences[0].cues.iter().enumerate() {
            let before = running.clone();
            for (f, vals) in &c.values {
                for (a, v) in vals {
                    running.insert(show.slot(*f, *a).unwrap(), l.resolve(*f, *a, *v).unwrap());
                }
            }
            let want: Vec<Entry> = running.iter().map(|(s, v)| Entry { slot: *s, value: *v }).collect();
            assert_eq!(seq.state(k), want, "cue {k}");
            if c.cue_only {
                running = before;
            }
        }
        assert_eq!(seq.snapshots.len(), 3);
        assert!(seq.cues[5].changes.len() <= 2, "a cue keeps only what it changes");
        // GO through the snapshot boundary.
        let t0 = Instant::now();
        let mut p = Playback::new(Arc::new(show), t0);
        p.go(1, Some(l.sequences[0].cues[SNAPSHOT + 1].id), t0).unwrap();
        assert_eq!(p.tick(at(t0, 0.1)).get(1).unwrap().data[9], 191, "pan 0.75 tracked from cue 11");
    }

    /// An edit recompiles only the sequence it touched; presets changing recompile everything.
    #[test]
    fn incremental_compile_keeps_unchanged_sequences() {
        let mut l = design();
        let a = LightShow::compile(&l);
        let prev = l.clone();
        l.sequences[0].cues[0].fade = 5.0;
        let b = LightShow::compile_from(&l, Some((&prev, &a)));
        assert!(!Arc::ptr_eq(&a.sequences[0], &b.sequences[0]) && Arc::ptr_eq(&a.sequences[1], &b.sequences[1]));
        assert_eq!(b.sequences[0].cues[0].fade, 5.0);
        let prev = l.clone();
        l.presets.push(Default::default());
        let c = LightShow::compile_from(&l, Some((&prev, &b)));
        assert!(!Arc::ptr_eq(&b.sequences[1], &c.sequences[1]));
        // Moving a fixture doesn't change what plays.
        let mut moved = l.clone();
        moved.fixtures[0].position = [1.0, 2.0, 3.0];
        assert!(LightShow::same_input(&l, &moved));
        moved.fixtures[0].address = 300;
        assert!(!LightShow::same_input(&l, &moved));
        // ...unless there are focus points: cues aimed at them follow the fixture.
        l.points.push(Default::default());
        let mut moved = l.clone();
        moved.fixtures[0].position[1] += 1.0;
        assert!(!LightShow::same_input(&l, &moved));
        let prev = l.clone();
        let d = LightShow::compile_from(&moved, Some((&prev, &c)));
        assert!(!Arc::ptr_eq(&c.sequences[0], &d.sequences[0]), "recompiled");
    }

    /// A quick second GO doesn't cut the effects still fading out from the GO before it.
    #[test]
    fn quick_gos_keep_effects_fading_out() {
        use crate::light::effects::{Effect, EffectRun, Shape};
        let t0 = Instant::now();
        let mut l = design();
        l.effects.push(Effect { id: 1, shape: Shape::Square, speed: 1.0, spread: 0.0, ..Default::default() });
        let start = LightCue { fade: 0.0, effects: vec![EffectRun { effect: 1, fixtures: vec![1], stop: false }], ..Default::default() };
        let stop = LightCue { fade: 2.0, effects: vec![EffectRun { effect: 1, stop: true, ..Default::default() }], ..Default::default() };
        let next = LightCue { fade: 2.0, ..Default::default() };
        l.sequences.push(Sequence { id: 3, cues: vec![start, stop, next].into(), ..Default::default() });
        let mut p = Playback::new(Arc::new(LightShow::compile(&l)), t0);
        p.go(3, None, t0).unwrap();
        p.go(3, None, at(t0, 10.0)).unwrap(); // effect fades out over 2 s
        p.go(3, None, at(t0, 10.5)).unwrap();
        assert!(p.tick(at(t0, 11.25)).get(1).unwrap().data[0] > 0, "still fading out");
        assert_eq!(p.tick(at(t0, 12.25)).get(1).unwrap().data[0], 0, "gone after its fade");
    }

    #[test]
    fn tick_does_not_reallocate() {
        let t0 = Instant::now();
        let mut p = Playback::new(Arc::new(LightShow::compile(&design())), t0);
        p.go(1, None, t0).unwrap();
        p.tick(t0);
        let caps = (p.values.capacity(), p.frame.universes.capacity(), p.seqs[0].tracks.capacity());
        for i in 0..100 {
            p.tick(at(t0, i as f64 * 0.05));
        }
        assert_eq!(caps, (p.values.capacity(), p.frame.universes.capacity(), p.seqs[0].tracks.capacity()));
    }
}
