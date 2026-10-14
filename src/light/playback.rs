//! Lighting playback: the compiled show ([`LightShow`]) and the state machine that turns GOs
//! into DMX frames ([`Playback`]). Pure logic, no I/O or threads (see [`super::engine`]).
//!
//! * Every (fixture, attribute) pair is a *slot*; values are normalised `f32`.
//! * A sequence GO builds one fade *track* per slot it touches: from the slot's current value to
//!   the cue's tracked value, after the cue's delay, over its fade (down fade for intensity going
//!   down). Slots the new cue doesn't hold are released (intensity to 0, others home).
//! * Resolve: intensity is highest-takes-precedence across sequences (scaled by each sequence's
//!   level), every other attribute latest-takes-precedence (a slot belongs to the sequence that
//!   last *changed* it). The programmer overrides both; the grand master scales intensity last.
//! * `tick` doesn't allocate once the show is loaded; GO only reuses buffers.

use super::dmx::DmxFrame;
use super::model::{Attribute, ChannelDef, FixtureId, LightCueId, Lighting, SequenceId, MAX_UNIVERSE, UNIVERSE_SIZE};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

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

/// A lighting cue ready for playback: timing plus its full tracked state.
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
    /// Tracked state, sorted by slot.
    pub state: Vec<(u32, f32)>,
}

impl CompiledCue {
    /// Seconds from GO until every fade of this cue is complete.
    pub fn duration(&self) -> f64 {
        self.delay + self.fade.max(self.down_fade)
    }
}

/// A sequence ready for playback.
#[derive(Clone, Debug)]
pub struct CompiledSequence {
    /// Design id.
    pub id: SequenceId,
    /// Cues in order.
    pub cues: Vec<CompiledCue>,
}

/// The read-only playback form of a [`Lighting`] design.
#[derive(Clone, Debug, Default)]
pub struct LightShow {
    /// Every slot.
    pub slots: Vec<SlotInfo>,
    index: HashMap<(FixtureId, Attribute), u32>,
    fixtures: Vec<FixtureOut>,
    /// Sequences.
    pub sequences: Vec<CompiledSequence>,
    /// Patched universes, sorted.
    pub universes: Vec<u16>,
}

impl LightShow {
    /// Compile a design. Fixtures with an unknown type or a duplicate number are skipped;
    /// fixtures outside a valid universe/address get slots but no output.
    pub fn compile(l: &Lighting) -> Self {
        let mut show = LightShow::default();
        let mut seen = HashSet::new();
        for f in &l.fixtures {
            let Some(mode) = l.mode_of(f) else { continue };
            if !seen.insert(f.id) {
                continue;
            }
            for attr in mode.attributes() {
                let home = mode.channel(attr).map_or(attr.home(), |c| c.default_value());
                show.index.insert((f.id, attr), show.slots.len() as u32);
                show.slots.push(SlotInfo { fixture: f.id, attr, home });
            }
            let footprint = mode.footprint();
            let valid = f.address > 0
                && (1..=MAX_UNIVERSE).contains(&f.universe)
                && f.address as u32 + footprint as u32 - 1 <= UNIVERSE_SIZE as u32;
            if valid {
                let channels = mode.channels.iter().map(|c| (c.clone(), show.index[&(f.id, c.attribute)])).collect();
                let dimmer = if mode.has_dimmer() { None } else { show.index.get(&(f.id, Attribute::Dimmer)).copied() };
                show.fixtures.push(FixtureOut {
                    universe: f.universe,
                    start: f.address as usize - 1,
                    channels,
                    dimmer,
                    invert_pan: f.invert_pan,
                    invert_tilt: f.invert_tilt,
                });
                show.universes.push(f.universe);
            }
        }
        show.universes.sort();
        show.universes.dedup();
        for s in &l.sequences {
            let mut running: BTreeMap<u32, f32> = BTreeMap::new();
            let cues = s
                .cues
                .iter()
                .map(|c| {
                    for (fixture, values) in &c.values {
                        for (attr, v) in values {
                            if let (Some(&slot), Some(v)) = (show.index.get(&(*fixture, *attr)), l.resolve(*fixture, *attr, *v)) {
                                running.insert(slot, v.clamp(0.0, 1.0));
                            }
                        }
                    }
                    CompiledCue {
                        id: c.id,
                        fade: c.fade.max(0.0),
                        down_fade: c.down_fade.unwrap_or(c.fade).max(0.0),
                        delay: c.delay.max(0.0),
                        state: running.iter().map(|(k, v)| (*k, *v)).collect(),
                    }
                })
                .collect();
            show.sequences.push(CompiledSequence { id: s.id, cues });
        }
        show
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

#[derive(Clone, Copy, Debug)]
struct Track {
    slot: u32,
    from: f32,
    to: f32,
    start: f64,
    dur: f64,
    /// LTP order: when this sequence last changed the slot.
    stamp: u64,
    /// Fading out; dropped when done.
    release: bool,
}

impl Track {
    fn value(&self, t: f64) -> f32 {
        if t < self.start {
            self.from
        } else if self.dur <= 0.0 || t >= self.start + self.dur {
            self.to
        } else {
            let k = ((t - self.start) / self.dur) as f32;
            self.from + (self.to - self.from) * k
        }
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
}

impl Default for SeqState {
    fn default() -> Self {
        Self { cue: None, level: 1.0, tracks: Vec::new() }
    }
}

/// What a sequence is doing, for the UI.
#[derive(Clone, Debug, PartialEq)]
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
    values: Vec<f32>,
    best: Vec<u64>,
    programmer: Vec<Option<f32>>,
    scratch: Vec<Track>,
    /// Grand master, 0..=1 (scales intensity).
    pub grand_master: f32,
    /// Blackout: intensity 0.
    pub blackout: bool,
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
            best: Vec::new(),
            programmer: Vec::new(),
            scratch: Vec::new(),
            grand_master: 1.0,
            blackout: false,
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
            n.cue = ostate.cue.and_then(|c| os.cues.get(c)).and_then(|c| self.show.sequences[ni].cues.iter().position(|x| x.id == c.id));
            n.tracks = ostate
                .tracks
                .iter()
                .filter_map(|t| {
                    let s = &old.slots[t.slot as usize];
                    self.show.slot(s.fixture, s.attr).map(|slot| Track { slot, ..*t })
                })
                .collect();
            n.tracks.sort_by_key(|t| t.slot);
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
        let mut values: Vec<f32> = self.show.slots.iter().map(|s| s.home).collect();
        for (i, v) in self.values.iter().enumerate() {
            if let Some(slot) = old.slots.get(i).and_then(|s| self.show.slot(s.fixture, s.attr)) {
                values[slot as usize] = *v;
            }
        }
        self.values = values;
        self.best = vec![0; self.show.slots.len()];
        self.frame = DmxFrame::default();
        for u in &self.show.universes {
            self.frame.get_mut(*u);
        }
    }

    /// GO a cue of a sequence (`cue` None = the next one). Returns the cue fired.
    pub fn go(&mut self, seq: SequenceId, cue: Option<LightCueId>, at: Instant) -> Result<LightCueId, String> {
        let si = self.show.sequences.iter().position(|s| s.id == seq).ok_or("unknown sequence")?;
        let sequence = &self.show.sequences[si];
        let ci = match cue {
            Some(id) => sequence.cues.iter().position(|c| c.id == id).ok_or("unknown lighting cue")?,
            None => self.seqs[si].cue.map_or(0, |c| c + 1),
        };
        let compiled = sequence.cues.get(ci).ok_or("end of sequence")?;
        let t = self.time(at);
        let show = self.show.clone();
        let st = &mut self.seqs[si];
        std::mem::swap(&mut st.tracks, &mut self.scratch);
        st.tracks.clear();
        let (old, new) = (&self.scratch, &compiled.state);
        let (mut i, mut j) = (0, 0);
        while i < old.len() || j < new.len() {
            let os = old.get(i).map_or(u32::MAX, |t| t.slot);
            let ns = new.get(j).map_or(u32::MAX, |n| n.0);
            let slot = os.min(ns);
            let o = (os == slot).then(|| old[i]);
            let n = (ns == slot).then(|| new[j].1);
            i += o.is_some() as usize;
            j += n.is_some() as usize;
            let info = &show.slots[slot as usize];
            let htp = info.attr.is_htp();
            let from = match o {
                Some(o) => o.value(t),
                None if htp => 0.0,
                None => self.values[slot as usize],
            };
            let (to, release) = match n {
                Some(v) => (v, false),
                None => (if htp { 0.0 } else { info.home }, true),
            };
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
            let dur = if htp && to < from { compiled.down_fade } else { compiled.fade };
            st.tracks.push(Track { slot, from, to, start: t + compiled.delay, dur, stamp, release });
        }
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
            *tr = Track {
                from: tr.value(t),
                to: if info.attr.is_htp() { 0.0 } else { info.home },
                start: t,
                dur: fade.max(0.0),
                release: true,
                ..*tr
            };
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

    /// Sequence states, for the UI.
    pub fn status(&self) -> Vec<SequenceStatus> {
        self.show
            .sequences
            .iter()
            .zip(&self.seqs)
            .map(|(s, st)| SequenceStatus {
                id: s.id,
                cue: st.cue.map(|c| s.cues[c].id),
                active: !st.tracks.is_empty(),
                level: st.level,
            })
            .collect()
    }

    /// Resolved value of a slot after the last tick (before the grand master).
    pub fn value(&self, slot: u32) -> Option<f32> {
        self.values.get(slot as usize).copied()
    }

    /// Advance to `now` and build the DMX frame.
    pub fn tick(&mut self, now: Instant) -> &DmxFrame {
        let t = self.time(now);
        let show = &*self.show;
        for (v, s) in self.values.iter_mut().zip(&show.slots) {
            *v = if s.attr.is_htp() { 0.0 } else { s.home };
        }
        self.best.fill(0);
        for st in &mut self.seqs {
            for tr in &st.tracks {
                let i = tr.slot as usize;
                let v = tr.value(t);
                if show.slots[i].attr.is_htp() {
                    self.values[i] = self.values[i].max(v * st.level);
                } else if tr.stamp >= self.best[i] {
                    self.values[i] = v;
                    self.best[i] = tr.stamp.max(1);
                }
            }
            st.tracks.retain(|tr| !(tr.release && tr.done(t)));
        }
        for (v, p) in self.values.iter_mut().zip(&self.programmer) {
            if let Some(p) = p {
                *v = *p;
            }
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
            ],
        });
        l.sequences.push(Sequence { id: 2, name: "Sub".into(), cues: vec![cue(&[(1, Attribute::Dimmer, 0.4), (3, Attribute::Pan, 0.25)], 0.0)] });
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
    fn cues_follow_preset_edits() {
        use crate::light::model::{Preset, Value};
        let t0 = Instant::now();
        let mut l = design();
        l.presets.push(Preset { id: 1, values: [(2, [(Attribute::Red, 0.2)].into_iter().collect())].into_iter().collect(), ..Default::default() });
        l.sequences[0].cues[0].values.get_mut(&2).unwrap().insert(Attribute::Red, Value::Preset { preset: 1 });
        let mut p = Playback::new(Arc::new(LightShow::compile(&l)), t0);
        p.go(1, None, t0).unwrap();
        assert_eq!(p.tick(at(t0, 5.0)).get(1).unwrap().data[1], 51);
        l.presets[0].values.get_mut(&2).unwrap().insert(Attribute::Red, 1.0);
        p.set_show(Arc::new(LightShow::compile(&l)));
        p.go(1, Some(l.sequences[0].cues[0].id), at(t0, 5.0)).unwrap();
        assert_eq!(p.tick(at(t0, 10.0)).get(1).unwrap().data[1], 255, "the cue follows the preset");
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
