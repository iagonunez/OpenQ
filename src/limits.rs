//! Plausible ranges for every number a show holds, and the functions that bring a show inside
//! them.
//!
//! The editor's controls already keep values sensible, but a show file, an autosave, a replica's
//! edit or an imported effect can hold anything JSON can write: a fixture 10¹² m away, a fade of a
//! thousand years, a 10⁹-pixel stage. Such values don't make sense in any venue and only break
//! things (plots that never finish, clocks that overflow, huge allocations), so every value that
//! comes from outside the editor goes through [`workspace`] (show files, autosaves, edits from
//! replicas both ways) or [`effect`] / [`fixture_type`] (library files). Values are clamped, not
//! refused: a show with one odd value still opens. NaN becomes the in-range value nearest 0. Long
//! lists are cut where a real show would never get.
//!
//! The ranges are those of the editor's own controls where they have one, a little wider
//! elsewhere.

use crate::animation::{AnimProp, Animation};
use crate::effects::{AudioEffect, VideoEffect};
use crate::light::model::{FixtureType, Lighting, Look, Shared, Value};
use crate::model::{Cue, CueKind, Geometry, MAX_OUT_CHANNELS, MAX_SRC_CHANNELS, Stage, Workspace};
use crate::spatial::AudioObject;

/// Most cues in a show, every list and group together. Measured on a desktop (2026-10): 50 000
/// cues cost about 1.7 ms of each UI frame and 60 ms to load (a 50 MB file), so they stay
/// comfortable, while the biggest real productions use a few thousand. Over it, a show is refused
/// (not cut short) and the editor won't add more.
pub const MAX_CUES: usize = 50_000;
/// Most lighting cues in a design, every sequence together.
pub const MAX_LIGHT_CUES: usize = 50_000;
/// Most stages: 16 displays (about the most one computer drives, with several graphics cards)
/// plus 2, which also leaves room for a surface spanning them all.
pub const MAX_STAGES: usize = 18;

/// Cues in `c` and its groups' children.
pub fn tree_size(c: &Cue) -> usize {
    1 + c.children().iter().map(tree_size).sum::<usize>()
}

/// Cues in the show, every list together.
pub fn cue_count(ws: &Workspace) -> usize {
    ws.cue_list.iter_all().len()
}

/// Lighting cues in the design, every sequence together.
pub fn light_cue_count(l: &Lighting) -> usize {
    l.sequences.iter().map(|s| s.cues.len()).sum()
}

/// Refuse a show with more cues, lighting cues or stages than OpenQ takes ([`MAX_CUES`],
/// [`MAX_LIGHT_CUES`], [`MAX_STAGES`]).
pub fn check_counts(ws: &Workspace) -> Result<(), String> {
    let n = cue_count(ws);
    if n > MAX_CUES {
        return Err(format!("The show has {n} cues; OpenQ works with at most {MAX_CUES}"));
    }
    let n = light_cue_count(&ws.lighting);
    if n > MAX_LIGHT_CUES {
        return Err(format!("The lighting design has {n} cues; OpenQ works with at most {MAX_LIGHT_CUES}"));
    }
    if ws.stages.len() > MAX_STAGES {
        return Err(format!("The show has {} stages; OpenQ works with at most {MAX_STAGES}", ws.stages.len()));
    }
    Ok(())
}

/// Longest time anywhere in a show (waits, durations, trims, slice and timecode times, animation
/// lengths): 24 hours, in seconds.
pub const MAX_TIME: f64 = 86_400.0;
/// Longest fade or delay (audio, video, lighting, panic), seconds: one hour.
pub const MAX_FADE: f64 = 3_600.0;
/// Quietest level stored, dB (anything below [`crate::model::MIN_DB`] is silence anyway).
pub const MIN_DB: f32 = -120.0;
/// Loudest level and output fader, dB.
pub const MAX_DB: f32 = 12.0;
/// Largest EQ boost or cut, dB.
pub const MAX_EQ_DB: f32 = 24.0;
/// EQ frequencies, Hz.
pub const EQ_FREQ: (f32, f32) = (20.0, 20_000.0);
/// EQ band Q.
pub const EQ_Q: (f32, f32) = (0.1, 18.0);
/// Playback rate (varispeed).
pub const RATE: (f32, f32) = (0.25, 4.0);
/// Farthest a fixture, a focus point, a plot shape or a label sits from the plot's origin, metres.
pub const MAX_DISTANCE: f32 = 200.0;
/// Farthest the plot editor moves a title or the legend, page units (the SVG of a 400 m plot is 24 000 px wide).
pub const MAX_PLOT_PAGE: f32 = 30_000.0;
/// Plot editor: most changed items (three per fixture, the fixture limit is the cue count) and added texts.
pub const MAX_PLOT_EDITS: usize = 4 * MAX_LIGHT_CUES;
/// Plot editor: most added texts.
pub const MAX_PLOT_NOTES: usize = 1_000;
/// Plot editor: longest text, characters.
pub const MAX_PLOT_TEXT: usize = 2_000;
/// Plot editor: text size (added texts and resized items).
pub const PLOT_TEXT_SIZE: (f32, f32) = (4.0, 200.0);
/// Largest rotation, degrees (ten turns, for animated spins).
pub const MAX_ROTATION: f32 = 3_600.0;
/// Speakers and sound objects: the room view's extent (room units, the speakers sit at ±1).
pub const ROOM: f32 = 1.25;
/// Picture position on its stage, in stage widths/heights from the centre.
pub const MAX_OFFSET: f32 = 10.0;
/// Largest picture scale.
pub const MAX_SCALE: f32 = 10.0;
/// Largest scale factor an animation key multiplies by.
pub const MAX_ANIM_SCALE: f32 = 100.0;
/// Stage canvas side, pixels.
pub const MAX_STAGE_SIDE: u32 = crate::remote::vet::MAX_STAGE_SIDE;
/// Stage corner positions, in window sizes (corners may sit outside the window for keystone).
pub const MAX_CORNER: f32 = 10.0;
/// Video layers.
pub const MAX_LAYER: i32 = 1_000;
/// Text size, points.
pub const TEXT_SIZE: (f32, f32) = (1.0, 2_000.0);
/// Text effects (outline, shadow, background padding and corner radius), points.
pub const MAX_TEXT_EFFECT: f32 = 500.0;
/// Longest text of a Text cue, characters.
pub const MAX_TEXT: usize = 100_000;
/// Camera capture size, pixels.
pub const CAMERA_SIDE: (u32, u32) = (16, 8_192);
/// Most entries in a cue's lists (EQ bands, effects, slices, animation tracks, objects...).
pub const MAX_ITEMS: usize = 256;
/// Most keys in one animation track.
pub const MAX_KEYS: usize = 10_000;
/// Times a file or slice plays.
pub const MAX_PLAYS: u32 = 9_999;
/// Double-GO protection, seconds.
pub const MAX_DOUBLE_GO: f64 = 10.0;
/// Speakers in the room.
pub const MAX_SPEAKERS: usize = 64;
/// Effect speed, cycles per second (or per beat).
pub const EFFECT_SPEED: (f32, f32) = (0.01, 50.0);
/// Effect phase spread, degrees.
pub const MAX_SPREAD: f32 = 1_440.0;
/// Fixture beam and moving range limits (pan/tilt range, beam angles), degrees.
pub const MAX_FIXTURE_ANGLE: f32 = 3_600.0;
/// Size of the playback buttons.
pub const BUTTON_SCALE: (f32, f32) = (0.25, 4.0);
/// Playback button pages.
pub const MAX_BUTTON_PAGES: usize = 100;
/// Focus points of a lighting design.
pub const MAX_POINTS: usize = 1_000;
/// Longest playback button page name, characters.
pub const MAX_PAGE_NAME: usize = 64;

/// `v` within `lo..=hi`; NaN becomes the value of the range nearest 0.
pub fn f32_in(v: &mut f32, lo: f32, hi: f32) {
    *v = if v.is_nan() { 0f32.clamp(lo, hi) } else { v.clamp(lo, hi) };
}

/// [`f32_in`] for `f64`.
pub fn f64_in(v: &mut f64, lo: f64, hi: f64) {
    *v = if v.is_nan() { 0f64.clamp(lo, hi) } else { v.clamp(lo, hi) };
}

fn time(v: &mut f64) {
    f64_in(v, 0.0, MAX_TIME);
}

fn fade(v: &mut f64) {
    f64_in(v, 0.0, MAX_FADE);
}

fn db(v: &mut f32) {
    f32_in(v, MIN_DB, MAX_DB);
}

fn unit(v: &mut f32) {
    f32_in(v, 0.0, 1.0);
}

/// Plot editor changes: offsets, texts, sizes and counts.
fn plot(p: &mut crate::light::model::PlotLayout) {
    let text = |t: &mut String| {
        if t.len() > MAX_PLOT_TEXT {
            *t = t.chars().take(MAX_PLOT_TEXT).collect();
        }
    };
    p.edits.truncate(MAX_PLOT_EDITS);
    for e in &mut p.edits {
        let max = if e.item.on_page() { MAX_PLOT_PAGE } else { MAX_DISTANCE };
        for v in &mut e.offset {
            f32_in(v, -max, max);
        }
        if let Some(t) = &mut e.text {
            text(t);
        }
        if let Some(v) = &mut e.size {
            f32_in(v, PLOT_TEXT_SIZE.0, PLOT_TEXT_SIZE.1);
        }
    }
    p.notes.truncate(MAX_PLOT_NOTES);
    for n in &mut p.notes {
        for v in &mut n.at {
            f32_in(v, -MAX_DISTANCE, MAX_DISTANCE);
        }
        text(&mut n.text);
        f32_in(&mut n.size, PLOT_TEXT_SIZE.0, PLOT_TEXT_SIZE.1);
    }
}

/// Bring every value of `ws` into its plausible range (see the module docs). Counts (cues,
/// stages) are checked by [`check_counts`].
pub fn workspace(ws: &mut Workspace) {
    for list in ws.cue_list.all_roots_mut() {
        for c in list.iter_mut() {
            cue(c);
        }
    }
    let s = &mut ws.settings;
    fade(&mut s.panic_fade);
    db(&mut s.master_volume_db);
    f64_in(&mut s.double_go_protection, 0.0, MAX_DOUBLE_GO);
    s.speakers.truncate(MAX_SPEAKERS);
    for sp in &mut s.speakers {
        f32_in(&mut sp.x, -ROOM, ROOM);
        f32_in(&mut sp.y, -ROOM, ROOM);
        sp.output = sp.output.min(MAX_OUT_CHANNELS as u16 - 1);
    }
    for st in &mut ws.stages {
        stage(st);
    }
    lighting(&mut ws.lighting);
}

/// Bring `st` into range: canvas size, corners, crop, blend.
pub fn stage(st: &mut Stage) {
    st.width = st.width.clamp(1, MAX_STAGE_SIDE);
    st.height = st.height.clamp(1, MAX_STAGE_SIDE);
    for c in st.corners.iter_mut().flatten() {
        f32_in(c, -MAX_CORNER, MAX_CORNER);
    }
    for v in st.crop.iter_mut().chain(&mut st.blend) {
        unit(v);
    }
    f32_in(&mut st.blend_gamma, 1.0, 3.0);
}

/// Bring a cue (and the cues of a group) into range.
pub fn cue(c: &mut Cue) {
    time(&mut c.pre_wait);
    time(&mut c.post_wait);
    if let Some(t) = &mut c.timecode_trigger {
        time(t);
    }
    match &mut c.kind {
        CueKind::Audio(a) => {
            db(&mut a.volume_db);
            f32_in(&mut a.pan, -1.0, 1.0);
            time(&mut a.start_time);
            if let Some(e) = &mut a.end_time {
                time(e);
            }
            a.play_count = a.play_count.clamp(1, MAX_PLAYS);
            f32_in(&mut a.rate, RATE.0, RATE.1);
            eq(&mut a.eq);
            routing(&mut a.routing, &mut a.out_levels);
            a.slices.truncate(MAX_KEYS);
            for s in &mut a.slices {
                time(&mut s.time);
                s.plays = s.plays.min(MAX_PLAYS);
            }
            fade(&mut a.fade_in);
            fade(&mut a.fade_out);
            audio_fx(&mut a.audio_fx);
            objects(&mut a.objects.objects);
        }
        CueKind::Video(v) => {
            db(&mut v.volume_db);
            unit(&mut v.opacity);
            v.layer = v.layer.clamp(-MAX_LAYER, MAX_LAYER);
            time(&mut v.start_time);
            if let Some(e) = &mut v.end_time {
                time(e);
            }
            v.play_count = v.play_count.clamp(1, MAX_PLAYS);
            geometry(&mut v.geometry);
            time(&mut v.still_duration);
            routing(&mut v.routing, &mut v.out_levels);
            animation(&mut v.animation);
            video_fx(&mut v.video_fx);
            audio_fx(&mut v.audio_fx);
            objects(&mut v.objects.objects);
        }
        CueKind::Text(t) => {
            if t.text.len() > MAX_TEXT {
                t.text = t.text.chars().take(MAX_TEXT).collect();
            }
            f32_in(&mut t.size, TEXT_SIZE.0, TEXT_SIZE.1);
            // 0 = from `bold`.
            if t.weight != 0 {
                t.weight = t.weight.clamp(100, 900);
            }
            f32_in(&mut t.letter_spacing, -0.5, 2.0);
            f32_in(&mut t.line_height, 0.5, 4.0);
            for v in [&mut t.outline_width, &mut t.shadow_blur, &mut t.bg_padding, &mut t.bg_radius] {
                f32_in(v, 0.0, MAX_TEXT_EFFECT);
            }
            for v in &mut t.shadow_offset {
                f32_in(v, -MAX_TEXT_EFFECT, MAX_TEXT_EFFECT);
            }
            geometry(&mut t.geometry);
            t.layer = t.layer.clamp(-MAX_LAYER, MAX_LAYER);
            unit(&mut t.opacity);
            time(&mut t.duration);
            animation(&mut t.animation);
            video_fx(&mut t.video_fx);
        }
        CueKind::Fade(f) => {
            fade(&mut f.duration);
            if let Some(v) = &mut f.volume_db {
                db(v);
            }
            if let Some(o) = &mut f.opacity {
                unit(o);
            }
            if let Some(g) = &mut f.geometry {
                geometry(g);
            }
            if let Some(o) = &mut f.objects {
                objects(o);
            }
            if let Some(v) = &mut f.video_fx {
                video_fx(v);
            }
        }
        CueKind::Wait { duration } => time(duration),
        CueKind::Light(l) => fade(&mut l.fade),
        CueKind::Mic(m) => {
            m.channels = m.channels.clamp(1, MAX_SRC_CHANNELS as u16);
            m.first_channel = m.first_channel.min(63);
            db(&mut m.volume_db);
            f32_in(&mut m.pan, -1.0, 1.0);
            eq(&mut m.eq);
            routing(&mut m.routing, &mut m.out_levels);
            time(&mut m.duration);
            fade(&mut m.fade_in);
            audio_fx(&mut m.audio_fx);
            objects(&mut m.objects.objects);
        }
        CueKind::Camera(c) => {
            if let Some(s) = &mut c.size {
                for v in s {
                    *v = (*v).clamp(CAMERA_SIDE.0, CAMERA_SIDE.1);
                }
            }
            unit(&mut c.opacity);
            c.layer = c.layer.clamp(-MAX_LAYER, MAX_LAYER);
            geometry(&mut c.geometry);
            time(&mut c.duration);
            video_fx(&mut c.video_fx);
        }
        CueKind::Group { children, .. } => {
            for ch in children {
                cue(ch);
            }
        }
        _ => {}
    }
}

/// Bring a picture's position, scale, rotation and crop into range.
pub fn geometry(g: &mut Geometry) {
    f32_in(&mut g.x, -MAX_OFFSET, MAX_OFFSET);
    f32_in(&mut g.y, -MAX_OFFSET, MAX_OFFSET);
    f32_in(&mut g.scale_x, 0.0, MAX_SCALE);
    f32_in(&mut g.scale_y, 0.0, MAX_SCALE);
    f32_in(&mut g.rotation, -MAX_ROTATION, MAX_ROTATION);
    for v in &mut g.crop {
        unit(v);
    }
}

fn eq(e: &mut crate::model::Eq) {
    e.bands.truncate(MAX_ITEMS);
    for b in &mut e.bands {
        f32_in(&mut b.freq, EQ_FREQ.0, EQ_FREQ.1);
        f32_in(&mut b.gain_db, -MAX_EQ_DB, MAX_EQ_DB);
        f32_in(&mut b.q, EQ_Q.0, EQ_Q.1);
    }
}

/// Routing matrix (dB) at most source × output channels, and the output faders.
fn routing(r: &mut Option<Vec<Vec<f32>>>, out_levels: &mut Vec<f32>) {
    if let Some(m) = r {
        m.truncate(MAX_SRC_CHANNELS);
        for row in m {
            row.truncate(MAX_OUT_CHANNELS);
            for v in row {
                db(v);
            }
        }
    }
    out_levels.truncate(MAX_OUT_CHANNELS);
    for v in out_levels {
        db(v);
    }
}

fn objects(o: &mut Vec<AudioObject>) {
    o.truncate(MAX_SRC_CHANNELS);
    for a in o {
        f32_in(&mut a.x, -ROOM, ROOM);
        f32_in(&mut a.y, -ROOM, ROOM);
        unit(&mut a.spread);
    }
}

fn audio_fx(fx: &mut Vec<AudioEffect>) {
    fx.truncate(MAX_ITEMS);
    for e in fx {
        for (v, spec) in e.params.iter_mut().zip(e.kind.params()) {
            f32_in(v, spec.min, spec.max);
        }
    }
}

fn video_fx(fx: &mut Vec<VideoEffect>) {
    fx.truncate(MAX_ITEMS);
    for e in fx {
        for (v, spec) in e.params.iter_mut().zip(e.kind.params()) {
            f32_in(v, spec.min, spec.max);
        }
    }
}

fn animation(a: &mut Animation) {
    time(&mut a.duration);
    a.tracks.truncate(MAX_ITEMS);
    for t in &mut a.tracks {
        t.keys.truncate(MAX_KEYS);
        // Values are relative: added offsets, scale factors, colours (see `AnimProp`).
        let (lo, hi) = match t.prop {
            AnimProp::X | AnimProp::Y => (-MAX_OFFSET, MAX_OFFSET),
            AnimProp::ScaleX | AnimProp::ScaleY => (0.0, MAX_ANIM_SCALE),
            AnimProp::Rotation => (-MAX_ROTATION, MAX_ROTATION),
            AnimProp::CropLeft | AnimProp::CropTop | AnimProp::CropRight | AnimProp::CropBottom => (-1.0, 1.0),
            _ => (0.0, 1.0),
        };
        for k in &mut t.keys {
            time(&mut k.t);
            for v in &mut k.v {
                f32_in(v, lo, hi);
            }
        }
    }
}

/// Change a copy-on-write part only if `f` changes something, so unchanged parts keep sharing
/// their allocation (the light engine reuses compiled sequences by that).
fn shared<T: Clone + PartialEq>(s: &mut Shared<T>, f: impl FnOnce(&mut T)) {
    let mut copy = s.clone();
    f(&mut copy);
    if *copy != **s {
        *s = copy;
    }
}

fn look(l: &mut Look) {
    for v in l.values_mut().flat_map(|a| a.values_mut()) {
        if let Value::Level(x) = v {
            unit(x);
        }
    }
}

/// Bring a lighting design into range: fixture and plot positions, fades, levels, effects.
pub fn lighting(l: &mut Lighting) {
    use crate::light::model::MAX_UNIVERSE;
    for t in &mut l.fixture_types {
        fixture_type(t);
    }
    shared(&mut l.fixtures, |fixtures| {
        for f in fixtures {
            f.universe = f.universe.clamp(1, MAX_UNIVERSE);
            f.address = f.address.min(512);
            for v in &mut f.position {
                f32_in(v, -MAX_DISTANCE, MAX_DISTANCE);
            }
            for v in &mut f.rotation {
                f32_in(v, -360.0, 360.0);
            }
        }
    });
    for s in &mut l.shapes {
        for v in s.a.iter_mut().chain(&mut s.b).chain(&mut s.label) {
            f32_in(v, -MAX_DISTANCE, MAX_DISTANCE);
        }
        f32_in(&mut s.height, -MAX_DISTANCE, MAX_DISTANCE);
    }
    l.points.truncate(MAX_POINTS);
    for p in &mut l.points {
        for v in &mut p.position {
            f32_in(v, -MAX_DISTANCE, MAX_DISTANCE);
        }
    }
    plot(&mut l.plot);
    for seq in &mut l.sequences {
        fade(&mut seq.mib_delay);
        fade(&mut seq.mib_fade);
        shared(&mut seq.cues, |cues| {
            for c in cues {
                fade(&mut c.fade);
                fade(&mut c.delay);
                if let Some(d) = &mut c.down_fade {
                    fade(d);
                }
                for ft in c.feature_times.values_mut() {
                    fade(&mut ft.fade);
                    fade(&mut ft.delay);
                }
                shared(&mut c.values, look);
            }
        });
    }
    for p in &mut l.presets {
        shared(&mut p.values, |values| {
            for v in values.values_mut().flat_map(|a| a.values_mut()) {
                unit(v);
            }
        });
    }
    for e in &mut l.effects {
        effect(e);
    }
    l.button_pages.truncate(MAX_BUTTON_PAGES);
    for p in &mut l.button_pages {
        if p.chars().count() > MAX_PAGE_NAME {
            *p = p.chars().take(MAX_PAGE_NAME).collect();
        }
    }
    let pages = l.button_page_count();
    for b in &mut l.buttons {
        fade(&mut b.fade);
        b.page = b.page.min(pages - 1);
    }
    f32_in(&mut l.button_scale, BUTTON_SCALE.0, BUTTON_SCALE.1);
    fade(&mut l.release_fade);
}

/// Bring an effect into range (speed, depth, phase, groups, audio reaction, formula sliders). A
/// custom table that doesn't check out is dropped (the effect falls back to a sine).
pub fn effect(e: &mut crate::light::effects::Effect) {
    use crate::light::effects::Shape;
    f32_in(&mut e.speed, EFFECT_SPEED.0, EFFECT_SPEED.1);
    unit(&mut e.size);
    unit(&mut e.base);
    unit(&mut e.duty);
    f32_in(&mut e.spread, 0.0, MAX_SPREAD);
    e.groups = e.groups.clamp(1, 64);
    e.wings = e.wings.clamp(1, 16);
    e.params = crate::light::effects::sanitize_params(std::mem::take(&mut e.params));
    if let Some(a) = &mut e.audio {
        f32_in(&mut a.gain, 0.0, 100.0);
        unit(&mut a.floor);
    }
    if e.table.as_ref().is_some_and(|t| t.validate().is_err()) {
        e.table = None;
    }
    if e.shape == Shape::Custom && e.table.is_none() {
        e.shape = Shape::Sine;
    }
}

/// Bring a fixture type's values into range (channel defaults, beam and moving ranges, colour
/// emitters). Its structure is checked by [`crate::light::fixlib::sanitize`].
pub fn fixture_type(t: &mut FixtureType) {
    for m in &mut t.modes {
        for c in m.channels.iter_mut().chain(m.heads.iter_mut().flat_map(|h| h.channels.iter_mut())) {
            // A default that isn't a number means none.
            for v in [&mut c.default, &mut c.highlight] {
                *v = v.filter(|x| !x.is_nan()).map(|x| x.clamp(0.0, 1.0));
            }
        }
    }
    let p = &mut t.physical;
    for v in [&mut p.pan_range, &mut p.tilt_range, &mut p.beam_min, &mut p.beam_max] {
        f32_in(v, 0.0, MAX_FIXTURE_ANGLE);
    }
    p.emitters.retain(|e| e.is_valid() && e.attribute.is_emitter());
    p.emitters.truncate(16);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AudioCue, Cue, CueKind};

    #[test]
    fn too_many_cues_or_stages_are_refused() {
        let mut ws = Workspace::default();
        ws.cue_list.cues = (0..MAX_CUES).map(|_| Cue::new(CueKind::Memo)).collect();
        assert!(check_counts(&ws).is_ok());
        let group = Cue::new(CueKind::Group { mode: crate::model::GroupMode::StartAll, children: vec![] });
        ws.cue_list.cues[0] = group;
        ws.cue_list.cues[0].children_mut().unwrap().push(Cue::new(CueKind::Memo));
        assert_eq!(tree_size(&ws.cue_list.cues[0]), 2);
        assert!(check_counts(&ws).is_err(), "group children count");
        let ws = Workspace { stages: (0..=MAX_STAGES).map(|i| Stage::new(&i.to_string())).collect(), ..Default::default() };
        assert!(check_counts(&ws).is_err());
    }

    #[test]
    fn wild_values_come_into_range() {
        let mut ws = Workspace::default();
        let mut c = Cue::new(CueKind::Audio(AudioCue { volume_db: f32::INFINITY, rate: 0.0, start_time: 1e300, ..Default::default() }));
        c.pre_wait = -5.0;
        c.post_wait = f64::NAN;
        ws.cue_list.cues.push(c);
        ws.stages[0].width = u32::MAX;
        ws.lighting = crate::light::library::bench_rig(2, 1, 1);
        ws.lighting.fixtures[0].position = [1e12, -1e12, f32::NAN];
        workspace(&mut ws);
        let c = &ws.cue_list.cues[0];
        assert_eq!((c.pre_wait, c.post_wait), (0.0, 0.0));
        let CueKind::Audio(a) = &c.kind else { unreachable!() };
        assert_eq!((a.volume_db, a.rate, a.start_time), (MAX_DB, RATE.0, MAX_TIME));
        assert_eq!(ws.stages[0].width, MAX_STAGE_SIDE);
        assert_eq!(ws.lighting.fixtures[0].position, [MAX_DISTANCE, -MAX_DISTANCE, 0.0]);
    }

    /// Values already in range are left alone, and so are the shared parts holding them.
    #[test]
    fn values_in_range_are_untouched() {
        let mut l = crate::light::library::bench_rig(20, 4, 10);
        let before = l.clone();
        lighting(&mut l);
        assert_eq!(l, before);
        assert!(Shared::ptr_eq(&l.fixtures, &before.fixtures));
        assert!(l.sequences.iter().zip(&before.sequences).all(|(a, b)| Shared::ptr_eq(&a.cues, &b.cues)));
    }
}
