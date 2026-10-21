//! Writes the big demo show used for manual testing, "The Lighthouse Keeper": a two-act play with
//! music, projections, live inputs and a full lighting rig. It is meant to be opened, played and
//! edited by hand, and touches every feature of the show model at least once (remote control
//! aside): every cue type, group mode, continue mode, audio/video effect, blend mode and fade kind,
//! slices with devamps, object audio, animation, several cue lists with a timecode list, stages
//! with a spanned and edge-blended surface, control surfaces, and a lighting design with custom
//! fixture types (named ranges, gobo pictures, cells, a second DMX break), presets, focus points, effects of
//! every shape (a formula one, an audio-reactive one, a tempo-synced one), tracking, cue-only
//! cues, a blocked cue, feature timing, move in black, DMX outputs and an input.
//!
//! `tools/demo_show.sh` downloads the media and runs this as `demo_show <dir>`: it reads
//! `<dir>/durations.tsv` (media path relative to `<dir>/media` → seconds) and writes
//! `<dir>/demo.openq` with 400 media cues (Audio, Video, Text, Mic, Camera) and 300 lighting cues.

use openq::animation::{AnimProp, Animation, Ease, Key, Repeat, Track};
use openq::effects::{AudioEffect, AudioFxKind, VideoEffect, VideoFxKind};
use openq::light::dmx::{OutputConfig, OutputProtocol, UniverseRoute};
use openq::light::dmxin::{InputConfig, InputProtocol, MergeMode};
use openq::light::effects::{AudioReact, Effect, EffectRun, Shape};
use openq::light::model::{
    LightButton, Look,
    cell_id, Attribute as A, ChannelDef, ChannelRange, DmxAddress, Feature, FeatureTime, Fixture, FixtureId, FixtureMode, FixtureType, Group,
    FocusPoint, Head, LightCue, Lighting, Physical, PlotItem, PlotNote, PlotShape, PlotSymbol, Preset, Sequence, ShapeKind, Value,
};
use openq::model::{
    AudioCue, BlendMode, CameraCue, ContinueMode, Cue, CueColor, CueId, CueKind, CueList, EqBand, EqKind, FadeCue, FadeCurve, Geometry, GroupMode,
    LightAction, LightingCue, ListInfo, MicCue, NetworkCue, OtherList, SliceMarker, Stage, StageId, TextAlign, TextCue, TimecodeRate,
    TimecodeSource, TimecodeSync, VideoCue, VideoFit, Workspace, MIN_DB,
};
use openq::spatial::{AudioObject, ObjectAudio, Speaker};
use openq::surface::{Mapping, SurfaceAction, SurfaceInput, SurfaceSettings};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

/// Media cues (Audio, Video, Text, Mic, Camera) in the show.
const MEDIA_CUES: usize = 400;
/// Lighting cues in the design.
const LIGHT_CUES: usize = 300;
/// Media cues per scene (the SFX pad takes what is left of `MEDIA_CUES`).
const SCENE_MEDIA: usize = 15;
/// Timecode the storm list starts at (10:00:00:00).
const TC_START: f64 = 36_000.0;

fn main() {
    let dir = PathBuf::from(std::env::args().nth(1).expect("usage: demo_show <dir>"));
    let dir = dir.canonicalize().expect("demo dir");
    let durations = std::fs::read_to_string(dir.join("durations.tsv")).expect("durations.tsv (run tools/demo_show.sh)");
    let dur: HashMap<String, f64> = durations
        .lines()
        .filter_map(|l| {
            let (p, d) = l.split_once('\t')?;
            Some((p.to_string(), d.trim().parse().ok()?))
        })
        .collect();
    let mut g = Gen::new(dir.join("media"), dur);
    let ws = g.build();
    let out = dir.join("demo.openq");
    ws.save(&out).expect("save");
    let (media, light_go) = count(&ws);
    let light_cues: usize = ws.lighting.sequences.iter().map(|s| s.cues.len()).sum();
    println!(
        "wrote {} — {media} media cues, {light_go} Light cues in the lists, {light_cues} lighting cues in {} sequences, {} fixtures",
        out.display(),
        ws.lighting.sequences.len(),
        ws.lighting.fixtures.len()
    );
    assert_eq!(media, MEDIA_CUES);
    assert_eq!(light_cues, LIGHT_CUES);
    assert!(ws.lighting.validate().is_empty(), "patch problems: {:?}", ws.lighting.validate());
}

/// (media cues, Light cues) over every list.
fn count(ws: &Workspace) -> (usize, usize) {
    let all = ws.cue_list.iter_all();
    let media = all.iter().filter(|c| c.kind.tag().is_media()).count();
    let light = all.iter().filter(|c| matches!(c.kind, CueKind::Light(_))).count();
    (media, light)
}

// ---------------------------------------------------------------------------------------------
// Scenes

#[derive(Clone, Copy, PartialEq)]
enum Mood {
    Storm,
    Dawn,
    Interior,
    Night,
    Village,
    Inn,
    Memory,
    Dark,
    Finale,
}

/// What happens in a scene; the generator turns it into cues.
struct Scene {
    title: &'static str,
    notes: &'static str,
    mood: Mood,
    /// Looping atmospheres (sfx stems).
    atmos: &'static [&'static str],
    /// Underscore (music stem).
    music: Option<&'static str>,
    /// Spot effects: (cue name, sfx stem).
    spots: &'static [(&'static str, &'static str)],
    /// Projection (path under media/).
    picture: Option<&'static str>,
    /// Scene-specific extras.
    extra: Extra,
}

#[derive(Clone, Copy, PartialEq)]
enum Extra {
    None,
    Storm,
    Lighthouse,
    Clock,
    Letter,
    Radio,
    Inquiry,
    Memories,
    Underwater,
    Curtain,
}

const ACT1: &[Scene] = &[
    Scene {
        title: "Prologue — The storm at sea",
        notes: "Houselights out on GO. Storm builds under the overture; ship's bell on the SM's call.",
        mood: Mood::Storm,
        atmos: &["waves_crashing_on_rock_beach", "strong_wind"],
        music: Some("celtic_impulse"),
        spots: &[("Thunder 1", "thunder_crack"), ("Ship's bell", "medium_bell_ringing_near"), ("Rolling thunder", "rolling_thunder"), ("Foghorn", "air_horn_in_close_hall_series")],
        picture: Some("video/sintel_trailer.mp4"),
        extra: Extra::Storm,
    },
    Scene {
        title: "The lighthouse at dawn",
        notes: "Slow dawn. Keeper enters SL with the lamp oil.",
        mood: Mood::Dawn,
        atmos: &["water_lapping_wind", "june_songbirds"],
        music: Some("gymnopedie_no_1"),
        spots: &[("Gull cry (crow)", "crow_call"), ("Door latch", "latch_opening_closing"), ("Footsteps on the stairs", "distant_footsteps_on_wood")],
        picture: Some("images/sintel_snow.jpg"),
        extra: Extra::Lighthouse,
    },
    Scene {
        title: "The keeper's kitchen",
        notes: "Interior. Clock on the mantel (practical); fire flicker in the grate.",
        mood: Mood::Interior,
        atmos: &["fire", "clock_ticking"],
        music: None,
        spots: &[("Cup on saucer", "cup_on_saucer"), ("Poker in the grate", "fireplace_tools"), ("Window shuts", "glosing_old_window"), ("Kettle lid", "cup_on_saucer")],
        picture: None,
        extra: Extra::Clock,
    },
    Scene {
        title: "A visitor at the door",
        notes: "Knock is called on the actor's look to the door, not the line.",
        mood: Mood::Interior,
        atmos: &["wind_quiet_creaks"],
        music: Some("comfortable_mystery"),
        spots: &[("Knock knock", "knock_on_wooden_door"), ("Door creaks open", "creaking_wooden_door"), ("Footsteps", "distant_footsteps_on_wood"), ("Door shuts", "heavy_door_close"), ("Key in lock", "key_in_lock")],
        picture: None,
        extra: Extra::None,
    },
    Scene {
        title: "The harbour village",
        notes: "Full company. Dock noise from the pit speakers.",
        mood: Mood::Village,
        atmos: &["crowd_talking", "dock_movement"],
        music: Some("carefree"),
        spots: &[("Church bell", "medium_bell_ringing_near"), ("Dog barks", "distant_dog_barking"), ("Crates crash", "debris_hits")],
        picture: Some("images/sintel_city.jpg"),
        extra: Extra::None,
    },
    Scene {
        title: "The village inn",
        notes: "Fiddle tune live from the band; playback only covers the scene change.",
        mood: Mood::Inn,
        atmos: &["murmuring_and_talking_men", "fire"],
        music: Some("celtic_impulse"),
        spots: &[("Glasses", "cup_on_saucer"), ("Plates smash", "broken_plates"), ("Cheer", "team_cheer"), ("Door bangs", "door_close_and_lock")],
        picture: None,
        extra: Extra::None,
    },
    Scene {
        title: "Night watch",
        notes: "Keeper alone on the gallery. Lamp sweep effect runs on the spots.",
        mood: Mood::Night,
        atmos: &["outside_night", "afternoon_crickets_long"],
        music: Some("long_note_two"),
        spots: &[("Owl", "owl_hooting"), ("Distant dog", "distant_dog_barking"), ("Wind gust", "wind")],
        picture: Some("images/lighthouse_logo.svg"),
        extra: Extra::Lighthouse,
    },
    Scene {
        title: "The letter",
        notes: "Narrator reads the letter live (Mic). Music sits under the voice.",
        mood: Mood::Memory,
        atmos: &["house_windy_day"],
        music: Some("dreamy_flashback"),
        spots: &[("Paper and pen (clock)", "mechanical_clock_ring"), ("Wind chimes", "wind_chimes")],
        picture: Some("images/sintel_title.png"),
        extra: Extra::Letter,
    },
    Scene {
        title: "Squall",
        notes: "Rain builds over 20 s. Lightning flashes are in the Storm FX sequence.",
        mood: Mood::Storm,
        atmos: &["rain_heavy_loud", "desert_howling_wind"],
        music: Some("volatile_reaction"),
        spots: &[("Thunder 2", "thunder_crack"), ("Low thunder", "low_thunder_rolls"), ("Shutter bangs", "door_close_and_lock"), ("Thunder 3", "rolling_thunder")],
        picture: Some("video/jellyfish_10s.mp4"),
        extra: Extra::Storm,
    },
    Scene {
        title: "Lamp failure",
        notes: "The lamp gutters and dies: Lighthouse sequence snaps to black on the crash.",
        mood: Mood::Dark,
        atmos: &["rain_on_roof", "gas_lamp_flickering"],
        music: Some("darkest_child"),
        spots: &[("Glass cracks", "glosing_old_window"), ("Lens crash", "crash"), ("Debris", "debris_hits")],
        picture: None,
        extra: Extra::Lighthouse,
    },
    Scene {
        title: "The flare",
        notes: "Pyro is a practical flare (LED) — sound covers the ignition. Radio voice live on Mic 3.",
        mood: Mood::Storm,
        atmos: &["thunderstorm_long"],
        music: Some("anguish"),
        spots: &[("Fuse", "fuse_ignite_and_burn"), ("Flare burns", "emergency_flare_burns"), ("Radio alert", "emergency_radio_alert")],
        picture: Some("video/sintel_10s.mp4"),
        extra: Extra::Radio,
    },
    Scene {
        title: "Rescue",
        notes: "End of Act One. Heroic Age plays out; blackout on the button, then interval.",
        mood: Mood::Finale,
        atmos: &["waves_crashing_on_rock_beach"],
        music: Some("heroic_age"),
        spots: &[("Crowd cheers", "team_cheer"), ("Timpani hit", "tympani_bing")],
        picture: Some("images/sintel_dragon.jpg"),
        extra: Extra::None,
    },
];

const ACT2: &[Scene] = &[
    Scene {
        title: "Aftermath",
        notes: "Grey morning. Rain thinning out.",
        mood: Mood::Dawn,
        atmos: &["light_rain", "water_lapping_wind"],
        music: Some("lightless_dawn"),
        spots: &[("Distant thunder", "distant_thunder"), ("Crow", "crow_call"), ("Door opens", "creaking_wooden_door")],
        picture: Some("images/sintel_snow.jpg"),
        extra: Extra::None,
    },
    Scene {
        title: "The inquiry",
        notes: "Live camera of the witness stand on the main screen.",
        mood: Mood::Village,
        atmos: &["voices_murmur"],
        music: None,
        spots: &[("Gavel (timpani)", "tympani_bing"), ("Murmur rises", "crowd_talking"), ("Door", "heavy_door_close")],
        picture: None,
        extra: Extra::Inquiry,
    },
    Scene {
        title: "Memories",
        notes: "Flashback: projections in sepia, fading to colour as the memory clears.",
        mood: Mood::Memory,
        atmos: &["small_stream_flowing", "june_songbirds"],
        music: Some("gymnopedie_no_1"),
        spots: &[("Children's chimes", "wind_chimes"), ("Slowed bell", "medium_bell_ringing_near")],
        picture: Some("video/big_buck_bunny_trailer.mov"),
        extra: Extra::Memories,
    },
    Scene {
        title: "The stranger's story",
        notes: "Ghost story told by the fire; lights pull in on the stranger.",
        mood: Mood::Inn,
        atmos: &["fire", "wind_through_window"],
        music: Some("ghost_story"),
        spots: &[("Owl", "owl_hooting"), ("Door creaks", "creaking_wooden_door"), ("Knock", "knock_on_wooden_door")],
        picture: None,
        extra: Extra::None,
    },
    Scene {
        title: "Market day",
        notes: "Comic scene. Sneaky Snitch on the thief's entrance.",
        mood: Mood::Village,
        atmos: &["crowd_talking", "june_songbirds"],
        music: Some("sneaky_snitch"),
        spots: &[("Drum roll", "drum_roll"), ("Plates", "broken_plates"), ("Cheer", "team_cheer"), ("Comic bump", "tympani_bing")],
        picture: Some("video/big_buck_bunny_10s.mp4"),
        extra: Extra::None,
    },
    Scene {
        title: "The broken lens",
        notes: "Keeper finds the cracked lens. Very still.",
        mood: Mood::Dark,
        atmos: &["wind_quiet_creaks"],
        music: Some("anguish"),
        spots: &[("Glass", "glosing_old_window"), ("Crash", "crash")],
        picture: None,
        extra: Extra::None,
    },
    Scene {
        title: "Storm returns",
        notes: "Storm sequence runs on timecode from the band's click (list 'Storm (MTC)').",
        mood: Mood::Storm,
        atmos: &["thunderstorm_long", "strong_wind"],
        music: Some("rites"),
        spots: &[("Thunder 4", "thunder_crack"), ("Thunder 5", "rolling_thunder"), ("Shutters", "door_close_and_lock"), ("Siren-like wind", "desert_howling_wind")],
        picture: Some("video/sintel_trailer.mp4"),
        extra: Extra::Storm,
    },
    Scene {
        title: "Into the dark",
        notes: "Underwater dream sequence: chorus on everything, jellyfish on the cyc.",
        mood: Mood::Dark,
        atmos: &["large_bubbles_surfacing", "air_woosh_underwater"],
        music: Some("darkest_child"),
        spots: &[("Bubbles", "large_bubbles_surfacing"), ("Rainstick", "rainstick_slow")],
        picture: Some("video/jellyfish_10s.mp4"),
        extra: Extra::Underwater,
    },
    Scene {
        title: "Dawn again",
        notes: "Lamp relit. Birdsong returns.",
        mood: Mood::Dawn,
        atmos: &["june_songbirds", "water_lapping_wind"],
        music: Some("crossing_the_chasm"),
        spots: &[("Lamp ignites (fuse)", "fuse_ignite_and_burn"), ("Bell", "medium_bell_ringing_near")],
        picture: Some("images/bunny_meadow.png"),
        extra: Extra::Lighthouse,
    },
    Scene {
        title: "Finale",
        notes: "Company number into bows. Curtain call sequence on executor 5.",
        mood: Mood::Finale,
        atmos: &["waves_crashing_on_rock_beach"],
        music: Some("heroic_age"),
        spots: &[("Cheer", "team_cheer"), ("Timpani", "tympani_bing")],
        picture: Some("images/lighthouse_logo.svg"),
        extra: Extra::Curtain,
    },
];

// ---------------------------------------------------------------------------------------------
// Generator

struct Gen {
    media: PathBuf,
    dur: HashMap<String, f64>,
    seed: u64,
    rig: Rig,
    main_stage: StageId,
    cyc_stage: StageId,
    gauze_stage: StageId,
    /// Lighting cues of each sequence still to be fired from the lists, in order.
    lx_next: BTreeMap<u32, usize>,
    /// Running cues the next scene change fades out.
    live_atmos: Vec<CueId>,
    live_music: Option<CueId>,
    live_picture: Option<CueId>,
    /// Interval music group (Preshow list), started from the end of Act One.
    interval_music: Option<CueId>,
    number: u32,
    /// Media cues made so far.
    made: usize,
}

impl Gen {
    fn new(media: PathBuf, dur: HashMap<String, f64>) -> Self {
        Self {
            media,
            dur,
            seed: 0x5eed_1157,
            rig: Rig::default(),
            main_stage: StageId::nil(),
            cyc_stage: StageId::nil(),
            gauze_stage: StageId::nil(),
            lx_next: BTreeMap::new(),
            live_atmos: Vec::new(),
            live_music: None,
            live_picture: None,
            interval_music: None,
            number: 0,
            made: 0,
        }
    }

    fn rand(&mut self) -> f32 {
        self.seed ^= self.seed << 13;
        self.seed ^= self.seed >> 7;
        self.seed ^= self.seed << 17;
        (self.seed % 10_000) as f32 / 10_000.0
    }

    fn range(&mut self, a: f32, b: f32) -> f32 {
        a + (b - a) * self.rand()
    }

    /// A media file: `rel` if it was downloaded, else another file of the same folder.
    fn file(&self, rel: &str) -> (PathBuf, f64) {
        if let Some(d) = self.dur.get(rel) {
            return (self.media.join(rel), *d);
        }
        // The same name in another format (sound effects are WAV or FLAC).
        let stem = rel.rsplit_once('.').map_or(rel, |(s, _)| s);
        for ext in ["wav", "flac", "ogg", "mp3"] {
            let other = format!("{stem}.{ext}");
            if let Some(d) = self.dur.get(&other) {
                return (self.media.join(other), *d);
            }
        }
        let folder = rel.split('/').next().unwrap_or("");
        let mut alt: Vec<(&String, &f64)> = self.dur.iter().filter(|(p, _)| p.starts_with(folder)).collect();
        alt.sort_by(|a, b| a.0.cmp(b.0));
        match alt.first() {
            Some((p, d)) => {
                eprintln!("missing {rel}, using {p}");
                (self.media.join(p), **d)
            }
            None if rel.starts_with("images/") && self.media.join(rel).exists() => (self.media.join(rel), 0.0),
            None => panic!("no media for {rel}: run tools/demo_show.sh"),
        }
    }

    fn next_number(&mut self) -> String {
        self.number += 1;
        self.number.to_string()
    }

    fn cue(&mut self, name: &str, kind: CueKind) -> Cue {
        let mut c = Cue::new(kind);
        c.name = name.into();
        c
    }

    fn numbered(&mut self, name: &str, kind: CueKind) -> Cue {
        self.made += usize::from(kind.tag().is_media());
        let mut c = self.cue(name, kind);
        c.number = self.next_number();
        c
    }

    fn audio(&mut self, name: &str, rel: &str, f: impl FnOnce(&mut AudioCue, f64)) -> Cue {
        let (path, d) = self.file(rel);
        let mut a = AudioCue { path, ..Default::default() };
        f(&mut a, d);
        self.numbered(name, CueKind::Audio(a))
    }

    fn sfx(&mut self, name: &str, stem: &str) -> Cue {
        self.sfx_at(name, stem, 0.0)
    }

    /// A spot effect; long recordings are trimmed to 8.5 s starting `at` (0..1) of the way in.
    fn sfx_at(&mut self, name: &str, stem: &str, at: f64) -> Cue {
        let vol = self.range(-12.0, -3.0);
        let pan = self.range(-0.6, 0.6);
        self.audio(name, &format!("sfx/{stem}.ogg"), |a, d| {
            a.volume_db = vol.round();
            a.pan = (pan * 10.0).round() / 10.0;
            if d > 12.0 {
                a.start_time = (((d - 10.0) * at).max(0.5) * 10.0).round() / 10.0;
                a.end_time = Some(a.start_time + 8.5);
                a.fade_out = 1.5;
            }
        })
    }

    /// A layer of the scene's sound design from the mood's pool, with a placement variant.
    fn texture(&mut self, mood: Mood, i: usize) -> Cue {
        let pool = texture_pool(mood);
        let stem = pool[(i * 3 + self.made) % pool.len()];
        let variant = ["close", "distant", "left", "right", "soft", "sting"][(i + self.made) % 6];
        let at = self.rand() as f64;
        let mut c = self.sfx_at(&format!("{} ({variant})", pretty(stem)), stem, at);
        if let CueKind::Audio(a) = &mut c.kind {
            match variant {
                "distant" => {
                    a.volume_db -= 8.0;
                    a.eq.enabled = true;
                    a.eq.bands[5].enabled = true;
                    a.eq.bands[5].freq = 3500.0;
                }
                "left" => a.pan = -0.8,
                "right" => a.pan = 0.8,
                "soft" => {
                    a.volume_db -= 4.0;
                    a.fade_in = 1.5;
                }
                "sting" => {
                    a.end_time = Some(a.start_time + 3.0);
                    a.fade_out = 0.5;
                }
                _ => {}
            }
        }
        c
    }

    fn atmos(&mut self, name: &str, stem: &str) -> Cue {
        let vol = self.range(-20.0, -12.0);
        self.audio(name, &format!("sfx/{stem}.ogg"), |a, d| {
            a.volume_db = vol.round();
            a.infinite_loop = true;
            a.fade_in = 4.0;
            a.fade_curve = FadeCurve::Logarithmic;
            if d > 30.0 {
                a.start_time = 2.0;
                a.end_time = Some((d - 2.0).min(120.0));
            }
            a.eq.enabled = true;
            a.eq.bands[0].enabled = true; // high-pass at 80 Hz keeps rumble out of the rig
        })
    }

    fn music(&mut self, name: &str, stem: &str) -> Cue {
        self.audio(name, &format!("music/{stem}.mp3"), |a, _| {
            a.volume_db = -8.0;
            a.fade_in = 2.0;
            a.fade_out = 6.0;
        })
    }

    fn video(&mut self, name: &str, rel: &str, f: impl FnOnce(&mut VideoCue)) -> Cue {
        let (path, _) = self.file(rel);
        let mut v = VideoCue { path, fit: VideoFit::Fill, stage: Some(self.main_stage), layer: 1, ..Default::default() };
        f(&mut v);
        self.numbered(name, CueKind::Video(v))
    }

    fn text(&mut self, name: &str, text: &str, f: impl FnOnce(&mut TextCue)) -> Cue {
        let mut t = TextCue { text: text.into(), stage: Some(self.gauze_stage), ..Default::default() };
        f(&mut t);
        self.numbered(name, CueKind::Text(t))
    }

    fn fade(&mut self, name: &str, target: CueId, f: impl FnOnce(&mut FadeCue)) -> Cue {
        let mut fc = FadeCue { target: Some(target), ..Default::default() };
        f(&mut fc);
        self.cue(name, CueKind::Fade(fc))
    }

    fn fade_out(&mut self, name: &str, target: CueId, secs: f64) -> Cue {
        self.fade(name, target, |f| {
            f.duration = secs;
            f.volume_db = Some(MIN_DB);
            f.stop_target_when_done = true;
        })
    }

    fn picture_out(&mut self, target: CueId) -> Cue {
        self.fade("Projection out", target, |f| {
            f.duration = 3.0;
            f.volume_db = Some(MIN_DB);
            f.opacity = Some(0.0);
            f.stop_target_when_done = true;
        })
    }

    fn group(&mut self, name: &str, mode: GroupMode, children: Vec<Cue>) -> Cue {
        self.numbered(name, CueKind::Group { mode, children })
    }

    fn memo(&mut self, name: &str, notes: &str) -> Cue {
        let mut c = self.cue(name, CueKind::Memo);
        c.notes = notes.into();
        c
    }

    /// A Light cue firing the next lighting cue of sequence `seq`.
    fn lx(&mut self, seq: u32) -> Cue {
        let i = self.lx_next.entry(seq).or_insert(0);
        let s = self.rig.l.sequence(seq).expect("sequence");
        let lc = s.cues.get(*i).unwrap_or_else(|| panic!("sequence {seq} has only {} cues", s.cues.len()));
        *i += 1;
        let (id, name) = (lc.id, format!("LX {}{} {}", if seq == 1 { String::new() } else { format!("{seq}/") }, lc.number, lc.name));
        let mut c = self.cue(name.trim_end(), CueKind::Light(LightingCue { action: LightAction::Go, cue: Some(id), sequence: None, fade: 0.0 }));
        c.number = self.next_number();
        c.color = CueColor::Yellow;
        c
    }

    fn lx_left(&self, seq: u32) -> usize {
        self.rig.l.sequence(seq).map_or(0, |s| s.cues.len()) - self.lx_next.get(&seq).copied().unwrap_or(0)
    }

    fn release(&mut self, name: &str, seq: Option<u32>, fade: f64) -> Cue {
        let mut c = self.numbered(name, CueKind::Light(LightingCue { action: LightAction::Release, cue: None, sequence: seq, fade }));
        c.color = CueColor::Yellow;
        c
    }

    fn build(&mut self) -> Workspace {
        let mut ws = Workspace { stages: self.stages(), settings: settings(), ..Default::default() };
        self.rig = Rig::build();

        // Lists: Act 1 is current; the others are tabs.
        self.number = 0;
        let act1 = self.act(ACT1, "Act One", 0);
        // The interval and act two carry on from where the lists left the running cues.
        self.number = 0;
        let preshow = self.preshow();
        self.number = 0;
        let act2 = self.act(ACT2, "Act Two", ACT1.len());
        self.number = 0;
        let storm = self.storm_list();
        self.number = 0;
        let pad = self.sfx_pad(MEDIA_CUES - self.made);
        // Act One's last cue starts the interval music in the Preshow list.
        let mut act1 = act1;
        let start = self.cue("Start interval music", CueKind::Start { target: self.interval_music });
        if let Some(CueKind::Group { children, .. }) = act1.last_mut().map(|c| &mut c.kind) {
            children.push(start);
        }
        assert_eq!(self.lx_left(1), 0, "every main sequence cue is fired from a list");

        let (mut act1, mut preshow, mut act2, mut storm, mut pad) = (act1, preshow, act2, storm, pad);
        for list in [&mut act1, &mut preshow, &mut act2, &mut storm, &mut pad] {
            renumber(list, &mut 0);
        }

        let mut lists = CueList::default();
        lists.cues = act1;
        lists.info = ListInfo { name: "Act One".into(), ..Default::default() };
        lists.pos = 1;
        lists.others = vec![
            OtherList { info: ListInfo { name: "Preshow & Interval".into(), ..Default::default() }, cues: preshow },
            OtherList { info: ListInfo { name: "Act Two".into(), ..Default::default() }, cues: act2 },
            OtherList {
                info: ListInfo {
                    name: "Storm (MTC)".into(),
                    timecode: TimecodeSync { source: TimecodeSource::Mtc, rate: TimecodeRate::Fps25, ..Default::default() },
                    ..Default::default()
                },
                cues: storm,
            },
            OtherList { info: ListInfo { name: "SFX pad".into(), ..Default::default() }, cues: pad },
        ];
        ws.cue_list = lists;
        ws.lighting = std::mem::take(&mut self.rig.l);
        ws
    }

    fn stages(&mut self) -> Vec<Stage> {
        let mut main = Stage::new("Main projection");
        // The projector hangs off-axis: a little keystone.
        main.corners = [[0.03, 0.02], [0.98, 0.0], [1.0, 1.0], [0.0, 0.97]];
        let mut cyc = Stage::new("Cyc surface");
        cyc.width = 3840;
        let mut left = Stage::new("Cyc L");
        left.span_of = Some(cyc.id);
        left.crop = [0.0, 0.0, 0.53, 1.0];
        left.blend = [0.0, 0.0, 0.06, 0.0];
        let mut right = Stage::new("Cyc R");
        right.span_of = Some(cyc.id);
        right.crop = [0.47, 0.0, 1.0, 1.0];
        right.blend = [0.06, 0.0, 0.0, 0.0];
        right.blend_gamma = 2.4;
        let mut gauze = Stage::new("Gauze (surtitles)");
        gauze.height = 540;
        self.main_stage = main.id;
        self.cyc_stage = cyc.id;
        self.gauze_stage = gauze.id;
        vec![main, cyc, left, right, gauze]
    }

    // -----------------------------------------------------------------------------------------
    // The acts

    fn act(&mut self, scenes: &[Scene], title: &str, first_scene: usize) -> Vec<Cue> {
        let mut out = Vec::new();
        let mut head = self.memo(&format!("{title} — standby"), "SM: standby all departments. Check the band's click is running before the Storm cues.");
        head.color = CueColor::Purple;
        out.push(head);
        let mut card = self.text(&format!("{title} (gauze)"), &title.to_uppercase(), |t| {
            t.size = 140.0;
            t.weight = 800;
            t.letter_spacing = 0.2;
            t.duration = 6.0;
            t.color = [255, 244, 214, 255];
            t.shadow_color = [0, 0, 0, 200];
            t.shadow_blur = 18.0;
            t.animation = Animation {
                duration: 6.0,
                repeat: Repeat::Once,
                tracks: vec![
                    Track { prop: AnimProp::Opacity, keys: vec![key(0.0, 0.0, Ease::Out), key(1.5, 1.0, Ease::Linear), key(4.5, 1.0, Ease::In), key(6.0, 0.0, Ease::Linear)] },
                    Track { prop: AnimProp::ScaleX, keys: vec![key(0.0, 0.92, Ease::Out), key(6.0, 1.05, Ease::Linear)] },
                    Track { prop: AnimProp::ScaleY, keys: vec![key(0.0, 0.92, Ease::Out), key(6.0, 1.05, Ease::Linear)] },
                ],
            };
        });
        card.continue_mode = ContinueMode::AutoContinue;
        card.post_wait = 0.5;
        out.push(card);
        out.push(self.lx_block_start());
        for (i, s) in scenes.iter().enumerate() {
            // Spread the main sequence evenly over the remaining scenes of both acts.
            let scenes_left = (ACT1.len() + ACT2.len()) - (first_scene + i);
            let lx = self.lx_left(1).div_ceil(scenes_left).min(self.lx_left(1));
            out.extend(self.scene(first_scene + i + 1, s, lx));
        }
        // End of the act: everything still running fades away.
        let mut end = Vec::new();
        for id in std::mem::take(&mut self.live_atmos) {
            end.push(self.fade_out("Atmos out", id, 6.0));
        }
        if let Some(id) = self.live_music.take() {
            end.push(self.fade_out("Music out", id, 8.0));
        }
        if let Some(id) = self.live_picture.take() {
            end.push(self.picture_out(id));
        }
        end.push(self.release("Release storm FX", Some(3), 4.0));
        let g = self.group(&format!("End of {title}"), GroupMode::StartAll, end);
        out.push(g);
        out
    }

    /// The act's first cue: the house goes out (sequence 2) as the act starts.
    fn lx_block_start(&mut self) -> Cue {
        let house = self.lx(2);
        let mut g = self.group("House out", GroupMode::StartFirstAndEnter, vec![house]);
        g.color = CueColor::Yellow;
        g
    }

    fn scene(&mut self, n: usize, s: &Scene, lx_count: usize) -> Vec<Cue> {
        let start_media = self.made;
        let extra_media = match s.extra {
            Extra::Storm | Extra::Inquiry | Extra::Memories => 3,
            Extra::Letter | Extra::Curtain => 2,
            Extra::Clock | Extra::Radio | Extra::Underwater => 1,
            Extra::Lighthouse | Extra::None => 0,
        };
        let mut out = Vec::new();
        let mut m = self.memo(&format!("Scene {n}: {}", s.title), s.notes);
        m.color = CueColor::Purple;
        out.push(m);

        // Transition: lights, atmos crossfade, projection change, music in.
        let mut tr = vec![self.lx(1)];
        for id in std::mem::take(&mut self.live_atmos) {
            let mut f = self.fade_out("Old atmos out", id, 5.0);
            f.pre_wait = 0.5;
            tr.push(f);
        }
        if let Some(id) = self.live_music.take() {
            tr.push(self.fade_out("Previous music out", id, 4.0));
        }
        for (k, stem) in s.atmos.iter().enumerate() {
            let mut a = self.atmos(&format!("Atmos: {}", pretty(stem)), stem);
            if k == 0 && n.is_multiple_of(3) {
                // A stereo atmosphere placed in the room instead of the routing matrix.
                if let CueKind::Audio(ac) = &mut a.kind {
                    ac.objects = ObjectAudio {
                        enabled: true,
                        objects: vec![AudioObject { x: -0.8, y: 0.6, spread: 0.3 }, AudioObject { x: 0.8, y: 0.6, spread: 0.3 }],
                    };
                }
            }
            self.live_atmos.push(a.id);
            tr.push(a);
        }
        if let Some(rel) = s.picture {
            if let Some(id) = self.live_picture.take() {
                tr.push(self.picture_out(id));
            }
            let pic = self.projection(n, rel, s.mood);
            self.live_picture = Some(pic[0].id);
            tr.extend(pic);
        }
        let g = self.group(&format!("Into scene {n}"), GroupMode::StartAll, tr);
        out.push(g);

        let mut music_id = None;
        if let Some(stem) = s.music {
            let mut mu = self.music(&format!("Underscore: {}", pretty(stem)), stem);
            if s.extra == Extra::Letter {
                // Played twice through (the letter is long).
                if let CueKind::Audio(a) = &mut mu.kind {
                    a.play_count = 2;
                    a.audio_fx = vec![AudioEffect::new(AudioFxKind::Compressor)];
                }
            }
            mu.pre_wait = 1.0;
            music_id = Some(mu.id);
            self.live_music = Some(mu.id);
            out.push(mu);
        }

        // The scene's own cues: spot effects interleaved with lighting cues.
        let mut spots = s.spots.iter();
        let mut lx_here = lx_count.saturating_sub(1); // the transition took one
        let mut step = 0;
        while lx_here > 0 || spots.len() > 0 {
            if let Some((name, stem)) = spots.next() {
                let mut c = self.sfx(name, stem);
                self.dress_spot(&mut c, s, step);
                if lx_here > 0 && step % 2 == 0 {
                    // Sound and light on one GO: the sound auto-continues into the LX.
                    c.continue_mode = ContinueMode::AutoContinue;
                    c.post_wait = 0.2;
                    out.push(c);
                    out.push(self.lx(1));
                    lx_here -= 1;
                } else {
                    out.push(c);
                }
            }
            if lx_here > 0 {
                out.push(self.lx(1));
                lx_here -= 1;
            }
            if spots.len() == 0 && self.made - start_media + extra_media < SCENE_MEDIA {
                let t = self.texture(s.mood, step);
                out.push(t);
            }
            if step == 1
                && let Some(id) = music_id
            {
                // Music dips under the dialogue.
                let curve = if n.is_multiple_of(2) { FadeCurve::Logarithmic } else { FadeCurve::Custom([0.1, 0.9]) };
                let mut f = self.fade("Music under dialogue", id, |f| {
                    f.duration = 4.0;
                    f.volume_db = Some(-24.0);
                    f.stop_target_when_done = false;
                    f.curve = curve;
                });
                f.number = self.next_number();
                out.push(f);
            }
            step += 1;
        }
        while self.made - start_media + extra_media < SCENE_MEDIA {
            let t = self.texture(s.mood, step);
            out.push(t);
            step += 1;
        }
        let extra = self.extras(n, s, &out);
        out.extend(extra);
        out
    }

    /// Per-cue variety: the features a sound designer reaches for, spread over the show.
    fn dress_spot(&mut self, c: &mut Cue, s: &Scene, step: usize) {
        let CueKind::Audio(a) = &mut c.kind else { return };
        let name = c.name.clone();
        match () {
            _ if name.starts_with("Thunder") => {
                a.volume_db = 0.0;
                a.out_levels = vec![0.0, 0.0, -3.0, -3.0];
                a.eq.enabled = true;
                a.eq.bands[1].gain_db = 4.0; // low shelf: weight
            }
            _ if name.contains("Owl") => a.audio_fx = vec![AudioEffect::new(AudioFxKind::Delay)],
            _ if name.contains("bell") || name.contains("Bell") => {
                a.audio_fx = vec![AudioEffect::new(AudioFxKind::Reverb)];
                if name.contains("Slowed") {
                    a.rate = 0.8;
                }
            }
            _ if name.starts_with("Knock") => a.play_count = 2,
            _ if name.contains("Radio") => {
                // Telephone EQ, distorted, and only out of the right-hand speaker (the radio set).
                a.eq.enabled = true;
                a.eq.bands = vec![
                    EqBand { enabled: true, kind: EqKind::HighPass, freq: 400.0, gain_db: 0.0, q: 0.707 },
                    EqBand { enabled: true, kind: EqKind::Peak, freq: 1500.0, gain_db: 6.0, q: 1.2 },
                    EqBand { enabled: true, kind: EqKind::LowPass, freq: 3500.0, gain_db: 0.0, q: 0.707 },
                ];
                a.audio_fx = vec![AudioEffect::new(AudioFxKind::Distortion)];
                a.routing = Some(vec![vec![MIN_DB, 0.0], vec![MIN_DB, 0.0]]);
            }
            _ if name.contains("Cheer") || name.contains("cheers") => {
                a.audio_fx = vec![AudioEffect::new(AudioFxKind::Limiter)];
                a.fade_curve = FadeCurve::Linear;
            }
            _ if s.extra == Extra::Underwater => a.audio_fx = vec![AudioEffect::new(AudioFxKind::Chorus)],
            _ if name.contains("Flare") => {
                // The flare drifts across the room (a Fade cue moves it later).
                a.objects = ObjectAudio { enabled: true, objects: vec![AudioObject { x: -0.9, y: -0.8, spread: 0.1 }, AudioObject { x: -0.7, y: -0.8, spread: 0.1 }] };
            }
            _ => {}
        }
        if step == 2 {
            c.pre_wait = 0.3;
        }
        if s.mood == Mood::Storm && step == 0 {
            c.color = CueColor::Blue;
        }
    }

    /// A projection that fades in: the picture cue (first) and its fade.
    fn projection(&mut self, n: usize, rel: &str, mood: Mood) -> Vec<Cue> {
        let still = rel.starts_with("images/");
        let blend = BlendMode::ALL[n % BlendMode::ALL.len()];
        let fit = [VideoFit::Fill, VideoFit::Fit, VideoFit::Stretch, VideoFit::Native][n % 4];
        let on_cyc = n % 2 == 1;
        let stage = if on_cyc { self.cyc_stage } else { self.main_stage };
        let mut fx = Vec::new();
        let kind = VideoFxKind::ALL[n % (VideoFxKind::ALL.len() - 1)];
        if mood == Mood::Memory {
            fx.push(VideoEffect::new(VideoFxKind::Sepia));
        } else {
            fx.push(VideoEffect::new(kind));
        }
        let pic = self.video(&format!("Projection: {}", pretty(rel.rsplit('/').next().unwrap_or(rel))), rel, |v| {
            v.opacity = 0.0;
            v.stage = Some(stage);
            v.blend_mode = blend;
            v.fit = fit;
            v.video_fx = fx;
            if still {
                v.still_duration = 0.0;
            } else {
                v.infinite_loop = !n.is_multiple_of(3);
                v.play_count = 2;
                v.hold_last_frame = n.is_multiple_of(3);
                v.volume_db = -18.0;
                v.audio_fx = vec![AudioEffect::new(AudioFxKind::Reverb)];
            }
            if n % 4 == 1 {
                v.geometry = Geometry { x: 0.05, y: -0.02, scale_x: 1.1, scale_y: 1.1, rotation: -2.0, crop: [0.0, 0.05, 0.0, 0.05] };
            }
            if n % 5 == 2 {
                // A slow drift that loops back and forth.
                v.animation = Animation {
                    duration: 20.0,
                    repeat: Repeat::PingPong,
                    tracks: vec![
                        Track { prop: AnimProp::X, keys: vec![key(0.0, -0.03, Ease::InOut), key(20.0, 0.03, Ease::InOut)] },
                        Track { prop: AnimProp::Tint, keys: vec![Key { t: 0.0, v: [1.0, 0.9, 0.8, 1.0], ease: Ease::Bezier([0.2, 0.0, 0.2, 1.0]) }, Key { t: 20.0, v: [0.8, 0.9, 1.0, 1.0], ease: Ease::Linear }] },
                    ],
                };
            }
        });
        let fade_in = self.fade("Projection in", pic.id, |f| {
            f.duration = 3.0;
            f.volume_db = None;
            f.opacity = Some(1.0);
            f.stop_target_when_done = false;
            f.curve = FadeCurve::Linear;
        });
        vec![pic, fade_in]
    }

    fn extras(&mut self, n: usize, s: &Scene, scene: &[Cue]) -> Vec<Cue> {
        let mut out = Vec::new();
        match s.extra {
            Extra::Storm => {
                // Lightning: three flashes from the Storm FX sequence, each with a thunder.
                for k in 0..3 {
                    if self.lx_left(3) <= 10 {
                        break; // the rest belong to the timecode list
                    }
                    let mut flash = self.lx(3);
                    flash.continue_mode = ContinueMode::AutoFollow;
                    let mut thunder = self.sfx(&format!("Thunder after flash {}", k + 1), if k % 2 == 0 { "thunder_crack" } else { "low_thunder_rolls" });
                    thunder.pre_wait = 0.4 + k as f64 * 0.3;
                    out.push(self.group(&format!("Lightning {}", k + 1), GroupMode::StartFirstAndEnter, vec![flash, thunder]));
                }
                out.push(self.release("Storm FX out", Some(3), 6.0));
            }
            Extra::Lighthouse => {
                // The lamp lights, sweeps, dies in the storm (three cues) and is relit at dawn (two).
                let n = match s.mood {
                    Mood::Dark => 3,
                    Mood::Dawn if self.lx_next.get(&4).copied().unwrap_or(0) > 0 => 2,
                    _ => 1,
                };
                for _ in 0..n.min(self.lx_left(4)) {
                    let mut l = self.lx(4);
                    l.notes = "The lamp: Lighthouse sequence on executor 4.".into();
                    out.push(l);
                }
            }
            Extra::Clock => {
                // The mantel clock vamps on a slice until the visitor knocks (Devamp), then
                // stops dead (Pause) and starts again later.
                let clock = self.audio("Mantel clock (vamp)", "sfx/clock_ticking.ogg", |a, d| {
                    a.volume_db = -10.0;
                    a.pan = -0.4;
                    a.start_time = 0.0;
                    a.end_time = Some(d.min(12.0));
                    a.slices = vec![SliceMarker { time: 2.0, plays: 1 }, SliceMarker { time: (d.min(12.0) * 0.6).max(4.0), plays: 0 }];
                    a.fade_out = 1.0;
                });
                let id = clock.id;
                out.push(clock);
                let mut dv = self.numbered("Clock: release the vamp", CueKind::Devamp { target: Some(id) });
                dv.notes = "GO on the visitor's knock.".into();
                out.push(dv);
                out.push(self.numbered("Clock stops", CueKind::Pause { target: Some(id) }));
                let mut w = self.numbered("Beat", CueKind::Wait { duration: 3.0 });
                w.continue_mode = ContinueMode::AutoFollow;
                out.push(w);
                out.push(self.numbered("Clock resumes", CueKind::Start { target: Some(id) }));
            }
            Extra::Letter => {
                let mut mic = self.numbered(
                    "Narrator (Mic 1)",
                    CueKind::Mic(MicCue {
                        volume_db: -3.0,
                        fade_in: 0.5,
                        audio_fx: vec![AudioEffect::new(AudioFxKind::Gate), AudioEffect::new(AudioFxKind::Compressor), AudioEffect::new(AudioFxKind::Reverb)],
                        objects: ObjectAudio { enabled: true, objects: vec![AudioObject { x: 0.0, y: -0.9, spread: 0.2 }] },
                        ..Default::default()
                    }),
                );
                mic.notes = "Default input. Narrator's radio mic; gate + compressor + a touch of room.".into();
                out.push(mic.clone());
                let mut words = self.text("Surtitle: the letter", "“If the light should fail, keep it burning in your heart.”", |t| {
                    t.size = 64.0;
                    t.weight = 400;
                    t.italic = true;
                    t.font = "DejaVu Serif".into();
                    t.line_height = 1.35;
                    t.bg_color = [0, 0, 0, 160];
                    t.bg_padding = 30.0;
                    t.bg_radius = 18.0;
                    t.geometry = Geometry { y: 0.32, ..Default::default() };
                    t.duration = 12.0;
                    t.animation = Animation {
                        duration: 12.0,
                        repeat: Repeat::Once,
                        tracks: vec![
                            Track { prop: AnimProp::TextColor, keys: vec![ckey(0.0, [1.0, 1.0, 1.0, 1.0]), ckey(10.0, [1.0, 0.85, 0.55, 1.0])] },
                            Track { prop: AnimProp::Background, keys: vec![ckey(0.0, [0.0, 0.0, 0.0, 0.0]), ckey(1.0, [0.0, 0.0, 0.0, 0.6])] },
                        ],
                    };
                });
                words.pre_wait = 1.0;
                out.push(words);
                out.push(self.fade_out("Narrator out", mic.id, 2.0));
            }
            Extra::Radio => {
                let mut mic = self.numbered(
                    "Radio voice (Mic 3, offstage)",
                    CueKind::Mic(MicCue {
                        first_channel: 0,
                        channels: 1,
                        volume_db: -6.0,
                        duration: 25.0,
                        eq: radio_eq(),
                        audio_fx: vec![AudioEffect::new(AudioFxKind::Bitcrusher), AudioEffect::new(AudioFxKind::Distortion)],
                        routing: Some(vec![vec![MIN_DB, 0.0]]),
                        out_levels: vec![0.0, -2.0],
                        ..Default::default()
                    }),
                );
                mic.notes = "Actor speaks into the offstage mic; sounds like a ship's radio from the set's radio (right).".into();
                out.push(mic);
                // The flare's sound drifts across the room as the flare is carried off.
                if let Some(flare) = find_named(scene, "Flare") {
                    let fl = self.fade("Flare drifts SR", flare, |f| {
                        f.duration = 8.0;
                        f.volume_db = None;
                        f.objects = Some(vec![AudioObject { x: 0.7, y: 0.5, spread: 0.4 }, AudioObject { x: 0.9, y: 0.5, spread: 0.4 }]);
                        f.stop_target_when_done = false;
                    });
                    out.push(fl);
                }
                let mut osc = self.numbered(
                    "Tell the band: tacet",
                    CueKind::Network(NetworkCue { host: "127.0.0.1".into(), port: 9000, address: "/band/cue".into(), args: "tacet 11 \"flare scene\"".into() }),
                );
                osc.notes = "OSC to the MD's click machine (port 9000).".into();
                out.push(osc);
            }
            Extra::Inquiry => {
                let main = self.main_stage;
                let cam = self.numbered(
                    "Witness camera (test pattern)",
                    CueKind::Camera(CameraCue {
                        device: Some("testsrc2=size=1280x720:rate=25".into()),
                        format: Some("lavfi".into()),
                        size: Some([1280, 720]),
                        opacity: 0.9,
                        layer: 5,
                        stage: Some(self.main_stage),
                        geometry: Geometry { x: 0.28, y: -0.25, scale_x: 0.4, scale_y: 0.4, ..Default::default() },
                        mirror: true,
                        video_fx: vec![VideoEffect::new(VideoFxKind::ChromaKey), VideoEffect::new(VideoFxKind::Vignette)],
                        fit: VideoFit::Fit,
                        ..Default::default()
                    }),
                );
                let id = cam.id;
                out.push(cam);
                let mut real = self.numbered(
                    "Witness camera (real camera)",
                    CueKind::Camera(CameraCue { stage: Some(self.main_stage), duration: 30.0, blend_mode: BlendMode::Screen, ..Default::default() }),
                );
                real.armed = false;
                real.notes = "Disarmed: arm it when a camera is plugged in (first camera found).".into();
                out.push(real);
                let mut stop = self.numbered("Camera off", CueKind::Stop { target: Some(id) });
                stop.pre_wait = 20.0;
                stop.continue_mode = ContinueMode::DoNotContinue;
                out.push(stop);
                let cap = self.text("Caption: witness", "WITNESS — THOMAS AVERY, HARBOUR MASTER", |t| {
                    t.size = 44.0;
                    t.weight = 600;
                    t.align = TextAlign::Left;
                    t.outline_width = 3.0;
                    t.outline_color = [20, 40, 80, 255];
                    t.stage = Some(main);
                    t.duration = 8.0;
                    t.geometry = Geometry { x: -0.2, y: 0.4, ..Default::default() };
                    t.blend_mode = BlendMode::Normal;
                    t.video_fx = vec![VideoEffect::new(VideoFxKind::Sharpen)];
                    t.animation = Animation {
                        duration: 1.0,
                        repeat: Repeat::Once,
                        tracks: vec![
                            Track { prop: AnimProp::CropRight, keys: vec![key(0.0, 1.0, Ease::Out), key(1.0, 0.0, Ease::Hold)] },
                            Track { prop: AnimProp::OutlineColor, keys: vec![ckey(0.0, [0.1, 0.2, 0.4, 1.0]), ckey(1.0, [0.2, 0.4, 0.8, 1.0])] },
                        ],
                    };
                });
                out.push(cap);
            }
            Extra::Memories => {
                // The flashback film turns from sepia to colour.
                if let Some(id) = self.live_picture {
                    let mut f = self.fade("Memory clears: sepia to colour", id, |f| {
                        f.duration = 10.0;
                        f.volume_db = None;
                        f.stop_target_when_done = false;
                        let mut sep = VideoEffect::new(VideoFxKind::Sepia);
                        sep.params[0] = 0.0;
                        f.video_fx = Some(vec![sep]);
                        f.geometry = Some(Geometry { scale_x: 1.15, scale_y: 1.15, ..Default::default() });
                    });
                    f.number = self.next_number();
                    f.pre_wait = 2.0;
                    out.push(f);
                }
                let choices: Vec<Cue> = ["kids_variant_a", "kids_variant_b", "kids_variant_c"]
                    .iter()
                    .zip(["wind_chimes", "june_songbirds", "small_stream_flowing"])
                    .map(|(n, stem)| self.sfx(&format!("Memory sting ({n})"), stem))
                    .collect();
                out.push(self.group("Memory sting (random)", GroupMode::StartRandom, choices));
            }
            Extra::Underwater => {
                let cyc = self.cyc_stage;
                let mut dream = self.video("Dream overlay", "video/jellyfish_10s.mp4", |v| {
                    v.stage = Some(cyc);
                    v.blend_mode = BlendMode::Screen;
                    v.layer = 3;
                    v.opacity = 0.6;
                    v.infinite_loop = true;
                    v.volume_db = MIN_DB;
                    v.video_fx = vec![
                        VideoEffect::new(VideoFxKind::Invert),
                        VideoEffect::new(VideoFxKind::Blur),
                        VideoEffect::new(VideoFxKind::Hue),
                        VideoEffect::new(VideoFxKind::Posterize),
                        VideoEffect::new(VideoFxKind::Exposure),
                        VideoEffect::new(VideoFxKind::Threshold),
                        VideoEffect::new(VideoFxKind::ColorControls),
                        VideoEffect::new(VideoFxKind::Pixellate),
                        VideoEffect::new(VideoFxKind::Colorize),
                    ];
                    for e in v.video_fx.iter_mut().skip(3) {
                        e.enabled = false; // stacked up, ready to try one by one
                    }
                    v.animation = Animation {
                        duration: 8.0,
                        repeat: Repeat::Loop,
                        tracks: vec![
                            Track { prop: AnimProp::Rotation, keys: vec![key(0.0, -3.0, Ease::InOut), key(4.0, 3.0, Ease::InOut), key(8.0, -3.0, Ease::Linear)] },
                            Track { prop: AnimProp::Y, keys: vec![key(0.0, 0.0, Ease::InOut), key(4.0, -0.04, Ease::InOut), key(8.0, 0.0, Ease::Linear)] },
                            Track { prop: AnimProp::CropTop, keys: vec![key(0.0, 0.0, Ease::Linear), key(8.0, 0.1, Ease::Linear)] },
                            Track { prop: AnimProp::CropBottom, keys: vec![key(0.0, 0.1, Ease::Linear), key(8.0, 0.0, Ease::Linear)] },
                            Track { prop: AnimProp::CropLeft, keys: vec![key(0.0, 0.0, Ease::Linear)] },
                            Track { prop: AnimProp::Opacity, keys: vec![key(0.0, 0.7, Ease::InOut), key(4.0, 1.0, Ease::InOut), key(8.0, 0.7, Ease::Linear)] },
                        ],
                    };
                });
                dream.notes = "Video FX stack: the first three are on; the rest are bypassed to try one at a time.".into();
                let id = dream.id;
                out.push(dream);
                let mut drift = self.fade("Dream drifts", id, |f| {
                    f.duration = 15.0;
                    f.volume_db = None;
                    f.geometry = Some(Geometry { x: -0.1, scale_x: 1.3, scale_y: 1.3, rotation: 8.0, crop: [0.1, 0.0, 0.0, 0.0], ..Default::default() });
                    f.stop_target_when_done = false;
                    f.curve = FadeCurve::SCurve;
                });
                drift.number = self.next_number();
                out.push(drift);
                let mut gone = self.fade("Dream fades", id, |f| {
                    f.duration = 6.0;
                    f.volume_db = None;
                    f.opacity = Some(0.0);
                    f.stop_target_when_done = true;
                });
                gone.number = self.next_number();
                gone.pre_wait = 10.0;
                out.push(gone);
            }
            Extra::Curtain => {
                let mut bows = Vec::new();
                while self.lx_left(5) > 0 {
                    let mut l = self.lx(5);
                    l.continue_mode = ContinueMode::DoNotContinue;
                    bows.push(l);
                }
                let mut g = self.group("Curtain call (bows)", GroupMode::StartFirstAndEnter, bows);
                g.notes = "Each bow is its own GO inside the group.".into();
                out.push(g);
                while self.lx_left(4) > 0 {
                    out.push(self.lx(4));
                }
                let applause = self.audio("Bow music: Heroic Age (from 1:00)", "music/heroic_age.mp3", |a, d| {
                    a.start_time = (d * 0.4).min(60.0);
                    a.volume_db = -4.0;
                    a.fade_in = 1.0;
                    a.fade_out = 5.0;
                    a.audio_fx = vec![AudioEffect::new(AudioFxKind::Limiter)];
                });
                out.push(applause);
                let main = self.main_stage;
                let mut exit = self.text("Thank you", "Thank you for coming", |t| {
                    t.size = 110.0;
                    t.weight = 300;
                    t.stage = Some(main);
                    t.duration = 0.0;
                    t.color = [255, 220, 150, 255];
                    t.shadow_color = [80, 30, 0, 180];
                    t.shadow_offset = [6.0, 6.0];
                    t.animation = Animation {
                        duration: 3.0,
                        repeat: Repeat::Loop,
                        tracks: vec![Track { prop: AnimProp::ShadowColor, keys: vec![ckey(0.0, [0.3, 0.1, 0.0, 0.7]), ckey(1.5, [0.6, 0.3, 0.0, 0.9]), ckey(3.0, [0.3, 0.1, 0.0, 0.7])] }],
                    };
                });
                exit.pre_wait = 4.0;
                out.push(exit);
                let mut house = self.lx(2);
                house.name = format!("{} (house up)", house.name);
                out.push(house);
                let mut stop = self.numbered("Stop everything", CueKind::Stop { target: None });
                stop.pre_wait = 30.0;
                stop.color = CueColor::Red;
                stop.notes = "Stop with no target: everything else stops (and lighting releases, see Lighting settings).".into();
                out.push(stop);
            }
            Extra::None => {}
        }
        let _ = n;
        out
    }

    // -----------------------------------------------------------------------------------------
    // The other lists

    fn preshow(&mut self) -> Vec<Cue> {
        let mut out = Vec::new();
        let mut m = self.memo("Preshow — house open", "FOH clearance from the house manager before Act One standby.");
        m.color = CueColor::Green;
        out.push(m);
        out.push(self.lx(2));
        // A playlist: each track follows the last; the group cue itself doesn't hold the playhead.
        let mut tracks = Vec::new();
        for stem in ["gymnopedie_no_1", "carefree", "local_forecast_-_elevator", "wallpaper", "easy_lemon", "comfortable_mystery"] {
            let mut t = self.music(&format!("Preshow: {}", pretty(stem)), stem);
            t.continue_mode = ContinueMode::AutoFollow;
            if let CueKind::Audio(a) = &mut t.kind {
                a.volume_db = -14.0;
                a.fade_in = 0.0;
                a.fade_out = 3.0;
                a.audio_fx = vec![AudioEffect::new(AudioFxKind::Compressor)];
            }
            tracks.push(t);
        }
        let first = tracks[0].id;
        // The last track follows into this: the playlist loops until it is faded out.
        let start_again = self.numbered("Play the first track again", CueKind::Start { target: Some(first) });
        tracks.push(start_again);
        let playlist = self.group("Preshow playlist", GroupMode::StartFirst, tracks);
        let playlist_id = playlist.id;
        out.push(playlist);
        let foyer = self.numbered(
            "Foyer camera to lobby screen",
            CueKind::Camera(CameraCue {
                device: Some("smptehdbars=size=1280x720".into()),
                format: Some("lavfi".into()),
                stage: Some(self.main_stage),
                opacity: 0.0,
                layer: 2,
                ..Default::default()
            }),
        );
        out.push(foyer);
        let mut phones = self.text("Phones off", "Please switch off your mobile phones", |t| {
            t.size = 72.0;
            t.weight = 500;
            t.duration = 10.0;
            t.color = [255, 255, 255, 255];
            t.bg_color = [180, 30, 30, 220];
            t.bg_radius = 40.0;
            t.bg_padding = 36.0;
        });
        phones.hotkey = Some("P".into());
        phones.midi_trigger = Some(openq::model::MidiTrigger { kind: openq::model::MidiTriggerKind::Note, channel: Some(9), number: 36 });
        out.push(phones);
        out.push(self.sfx("Phone ring (announcement)", "phone_alerts_and_rings"));
        let mut bell = self.sfx("Beginners bell", "medium_bell_ringing_near");
        if let CueKind::Audio(a) = &mut bell.kind {
            a.volume_db = -6.0;
            a.end_time = Some(4.0);
        }
        out.push(bell);
        let mut out_pl = self.fade("Preshow music out", playlist_id, |f| {
            f.duration = 6.0;
            f.volume_db = Some(MIN_DB);
            f.stop_target_when_done = true;
        });
        out_pl.number = self.next_number();
        out_pl.continue_mode = ContinueMode::AutoContinue;
        out_pl.post_wait = 3.0;
        out.push(out_pl);
        out.push(self.lx(2));

        // Interval.
        let mut m = self.memo("Interval", "20 minutes. Bar bells at 5 and 3 minutes; house to half at beginners.");
        m.color = CueColor::Green;
        let interval = m.id;
        let mut skip = self.numbered("Standby interval", CueKind::Goto { target: Some(interval) });
        skip.notes = "After Act One starts, the playhead waits at the interval.".into();
        out.push(skip);
        out.push(m);
        let mut ilx = self.lx(2);
        ilx.name = format!("{} (interval house)", ilx.name);
        out.push(ilx);
        let mut im = Vec::new();
        for stem in ["wallpaper", "easy_lemon", "local_forecast_-_elevator"] {
            let mut t = self.music(&format!("Interval: {}", pretty(stem)), stem);
            t.continue_mode = ContinueMode::AutoFollow;
            im.push(t);
        }
        let mut ig = self.group("Interval music", GroupMode::StartFirst, im);
        ig.notes = "Started from Act One's last cue (Start cue in another list).".into();
        self.interval_music = Some(ig.id);
        out.push(ig);
        let main = self.main_stage;
        let mut count = self.text("Interval countdown", "Act Two begins in 5 minutes", |t| {
            t.size = 60.0;
            t.stage = Some(main);
            t.duration = 300.0;
            t.color = [200, 230, 255, 255];
            t.bg_color = [10, 30, 60, 200];
            t.bg_padding = 20.0;
            t.animation = Animation {
                duration: 2.0,
                repeat: Repeat::PingPong,
                tracks: vec![Track { prop: AnimProp::Opacity, keys: vec![key(0.0, 0.75, Ease::InOut), key(2.0, 1.0, Ease::InOut)] }],
            };
        });
        count.post_wait = 120.0;
        count.continue_mode = ContinueMode::AutoContinue;
        out.push(count);
        let bells: Vec<Cue> = (0..3).map(|k| self.sfx(&format!("Bar bell {}", k + 1), "medium_bell_ringing_near")).collect();
        let mut bg = self.group("Bar bells (all)", GroupMode::StartAll, bells);
        bg.hotkey = Some("B".into());
        out.push(bg);
        let mut half = self.lx(2);
        half.name = format!("{} (beginners)", half.name);
        out.push(half);
        out
    }

    fn storm_list(&mut self) -> Vec<Cue> {
        let mut out = Vec::new();
        let mut m = self.memo("Storm sequence on timecode", "Follows MTC from the band's playback rig at 10:00:00:00. Nothing here needs a GO.");
        m.color = CueColor::Blue;
        out.push(m);
        let hits = [("thunder_crack", 2.0), ("rolling_thunder", 9.5), ("low_thunder_rolls", 17.0), ("thunder_crack", 23.0), ("distant_thunder", 31.0), ("thunder_crack", 38.0), ("rolling_thunder", 44.5), ("low_thunder_rolls", 52.0)];
        for (k, (stem, t)) in hits.iter().enumerate() {
            if self.lx_left(3) > 0 {
                let mut l = self.lx(3);
                l.timecode_trigger = Some(TC_START + t - 0.4);
                out.push(l);
            }
            let mut c = self.sfx(&format!("TC thunder {}", k + 1), stem);
            c.timecode_trigger = Some(TC_START + t);
            out.push(c);
        }
        while self.lx_left(3) > 0 {
            let mut l = self.lx(3);
            l.timecode_trigger = Some(TC_START + 60.0);
            out.push(l);
        }
        let mut rain = self.atmos("TC rain bed", "rain_heavy_loud");
        rain.timecode_trigger = Some(TC_START);
        let rid = rain.id;
        out.push(rain);
        let mut out_rain = self.fade_out("TC rain out", rid, 10.0);
        out_rain.timecode_trigger = Some(TC_START + 58.0);
        out.push(out_rain);
        out
    }

    /// Hotkey pad of spare effects, sized so the show has exactly `n` more media cues.
    fn sfx_pad(&mut self, n: usize) -> Vec<Cue> {
        assert!(n >= 20, "only {n} cues left for the pad: lower SCENE_MEDIA");
        let mut out = Vec::new();
        let mut m = self.memo("SFX pad", "Show mode: press the key on each cue. Spares for when something goes wrong.");
        m.color = CueColor::Orange;
        out.push(m);
        let mut stems: Vec<&str> = Vec::new();
        let mut files: Vec<&String> = self.dur.keys().filter(|p| p.starts_with("sfx/")).collect();
        files.sort();
        let names: Vec<String> = files.iter().map(|p| p.trim_start_matches("sfx/").rsplit_once('.').map_or(p.as_str(), |(s, _)| s).to_string()).collect();
        stems.extend(names.iter().map(String::as_str));
        let keys: Vec<String> = (1..=12).map(|k| format!("F{k}")).chain(('A'..='Z').filter(|c| !matches!(c, 'P' | 'B')).map(|c| c.to_string())).collect();
        let mut media = 0;
        // A few banks (groups) of related spares, then single cues.
        let bank: Vec<Cue> = ["door_close_and_lock", "heavy_door_close", "creaking_wooden_door", "knock_on_wooden_door"]
            .iter()
            .map(|s| self.sfx(&format!("Spare: {}", pretty(s)), s))
            .collect();
        media += bank.len();
        out.push(self.group("Door spares", GroupMode::StartFirstAndEnter, bank));
        let mut i = 0;
        while media < n {
            let stem = stems[i % stems.len()];
            let mut c = self.sfx(&format!("Spare: {}", pretty(stem)), stem);
            if let Some(k) = keys.get(i) {
                c.hotkey = Some(k.clone());
            }
            if i % 7 == 3 {
                c.color = CueColor::Orange;
            }
            if i % 11 == 5
                && let CueKind::Audio(a) = &mut c.kind
            {
                a.fade_curve = FadeCurve::Custom([0.8, 0.95]);
                a.fade_in = 0.3;
            }
            out.push(c);
            media += 1;
            i += 1;
        }
        let mut panic = self.numbered("PANIC: stop everything", CueKind::Stop { target: None });
        panic.hotkey = Some("Escape".into());
        panic.color = CueColor::Red;
        out.push(panic);
        out
    }
}

/// Number the numbered cues of a list in display order (groups are made after their children).
fn renumber(cues: &mut [Cue], n: &mut u32) {
    for c in cues {
        if !c.number.is_empty() {
            *n += 1;
            c.number = n.to_string();
        }
        if let Some(children) = c.children_mut() {
            renumber(children, n);
        }
    }
}

fn find_named(cues: &[Cue], part: &str) -> Option<CueId> {
    cues.iter().find(|c| c.name.contains(part)).map(|c| c.id)
}

fn key(t: f64, v: f32, ease: Ease) -> Key {
    Key { t, v: [v, 0.0, 0.0, 0.0], ease }
}

fn ckey(t: f64, v: [f32; 4]) -> Key {
    Key { t, v, ease: Ease::Linear }
}

fn radio_eq() -> openq::model::Eq {
    openq::model::Eq {
        enabled: true,
        bands: vec![
            EqBand { enabled: true, kind: EqKind::HighPass, freq: 350.0, gain_db: 0.0, q: 0.707 },
            EqBand { enabled: true, kind: EqKind::LowShelf, freq: 500.0, gain_db: -6.0, q: 0.707 },
            EqBand { enabled: true, kind: EqKind::HighShelf, freq: 3000.0, gain_db: 3.0, q: 0.707 },
            EqBand { enabled: true, kind: EqKind::LowPass, freq: 4000.0, gain_db: 0.0, q: 0.707 },
        ],
    }
}

/// "waves_crashing_on_rock_beach" → "Waves crashing on rock beach".
fn pretty(stem: &str) -> String {
    let s = stem.trim_end_matches(".jpg").trim_end_matches(".png").trim_end_matches(".svg").trim_end_matches(".mp4").trim_end_matches(".mov").replace(['_', '-'], " ");
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => s,
    }
}

fn settings() -> openq::model::Settings {
    let speakers = vec![
        Speaker { name: "Pros L".into(), output: 0, x: -1.0, y: -1.0 },
        Speaker { name: "Pros R".into(), output: 1, x: 1.0, y: -1.0 },
        Speaker { name: "Surround L".into(), output: 2, x: -1.0, y: 0.8 },
        Speaker { name: "Surround R".into(), output: 3, x: 1.0, y: 0.8 },
    ];
    let mut mappings = SurfaceSettings::executor_bank(5, 0, 1, 36);
    mappings.extend([
        Mapping { input: SurfaceInput::MidiCc { channel: 0, number: 7 }, action: SurfaceAction::GrandMaster },
        Mapping { input: SurfaceInput::MidiNote { channel: 0, number: 48 }, action: SurfaceAction::Blackout },
        Mapping { input: SurfaceInput::MidiNote { channel: 0, number: 49 }, action: SurfaceAction::ReleaseAll },
        Mapping { input: SurfaceInput::MidiNote { channel: 0, number: 50 }, action: SurfaceAction::TapTempo },
        Mapping { input: SurfaceInput::MidiNote { channel: 0, number: 51 }, action: SurfaceAction::ExecutorFlash(3) },
        Mapping { input: SurfaceInput::MidiNote { channel: 0, number: 52 }, action: SurfaceAction::ExecutorRelease(1) },
        Mapping { input: SurfaceInput::Osc("/surface/go".into()), action: SurfaceAction::Go },
        Mapping { input: SurfaceInput::Osc("/surface/stop".into()), action: SurfaceAction::Stop },
        Mapping { input: SurfaceInput::MidiNote { channel: 0, number: 53 }, action: SurfaceAction::PlaybackButton(1) },
        Mapping { input: SurfaceInput::MidiNote { channel: 0, number: 54 }, action: SurfaceAction::PagePrevious },
        Mapping { input: SurfaceInput::MidiNote { channel: 0, number: 55 }, action: SurfaceAction::PageNext },
        Mapping { input: SurfaceInput::MidiProgram { channel: 0, number: 0 }, action: SurfaceAction::Go },
    ]);
    openq::model::Settings {
        panic_fade: 2.0,
        master_volume_db: -3.0,
        double_go_protection: 0.3,
        osc_enabled: true,
        osc_port: 53000,
        speakers,
        surfaces: SurfaceSettings { mappings, midi_clock: true, page_size: 5, soft_takeover: true, osc_feedback_port: 9000 },
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------------------------
// Lighting

#[derive(Default)]
struct Rig {
    l: Lighting,
    foh: Vec<FixtureId>,
    over: Vec<FixtureId>,
    cyc: Vec<FixtureId>,
    spots: Vec<FixtureId>,
    washes: Vec<FixtureId>,
    battens: Vec<FixtureId>,
    cells: Vec<FixtureId>,
    beams: Vec<FixtureId>,
    strobes: Vec<FixtureId>,
    follow: Vec<FixtureId>,
    house: Vec<FixtureId>,
    fog: FixtureId,
    haze: FixtureId,
}

// Preset numbers.
const P_WARM: u32 = 1;
const P_MOON: u32 = 2;
const P_STORM: u32 = 3;
const P_SUNSET: u32 = 4;
const P_RED: u32 = 5;
const P_SEA: u32 = 6;
const P_LAVENDER: u32 = 7;
const P_CENTRE: u32 = 11;
const P_DSL: u32 = 12;
const P_DSR: u32 = 13;
const P_DOOR: u32 = 14;
const P_AUDIENCE: u32 = 15;
const P_LAMP: u32 = 16;
const P_WINDOW: u32 = 21;
const P_LEAVES: u32 = 22;
const P_TIGHT: u32 = 23;
const P_FLOOD: u32 = 24;
const P_FULL: u32 = 31;
// Focus points.
const PT_CENTRE: u32 = 1;
const PT_DSL: u32 = 2;
const PT_DSR: u32 = 3;
const PT_RISER: u32 = 4;

// Effect numbers.
const FX_CANDLE: u32 = 1;
const FX_WAVES: u32 = 2;
const FX_LIGHTNING: u32 = 3;
const FX_SWEEP: u32 = 4;
const FX_CHASE: u32 = 5;
const FX_PULSE: u32 = 6;
const FX_RAINBOW: u32 = 7;
const FX_HAZE: u32 = 8;
const FX_RAMP: u32 = 9;

impl Rig {
    fn build() -> Rig {
        let mut r = Rig::default();
        r.patch();
        r.groups();
        r.presets();
        r.points();
        r.effects();
        r.sequences();
        r.buttons();
        r.l.outputs = vec![
            OutputConfig {
                name: "Art-Net (localhost)".into(),
                enabled: true,
                protocol: OutputProtocol::ArtNet { target: "127.0.0.1".into(), sync: true },
                routes: (1..=4).map(|u| UniverseRoute { universe: u, wire: u - 1 }).collect(),
            },
            OutputConfig {
                name: "sACN to the dimmer rack".into(),
                enabled: false,
                protocol: OutputProtocol::Sacn { target: String::new(), priority: 120 },
                routes: vec![UniverseRoute { universe: 1, wire: 1 }],
            },
            OutputConfig {
                name: "USB DMX (backup)".into(),
                enabled: false,
                protocol: OutputProtocol::EnttecPro { port: "/dev/ttyUSB0".into() },
                routes: vec![UniverseRoute { universe: 1, wire: 1 }],
            },
        ];
        r.l.inputs = vec![InputConfig {
            name: "Backup desk (capture)".into(),
            enabled: true,
            protocol: InputProtocol::ArtNet,
            interface: String::new(),
            routes: vec![UniverseRoute { universe: 9, wire: 15 }],
            merge: MergeMode::Capture,
        }];
        r.l.stop_all_releases = true;
        r.l.release_fade = 2.0;
        r
    }

    fn add(&mut self, id: FixtureId, name: String, t: &FixtureType, universe: u16, pos: [f32; 3], rot: [f32; 3]) -> FixtureId {
        self.l.add_type(t);
        let mode = &t.modes[0];
        let address = self.l.next_free_address(universe, mode.footprint()).expect("universe full");
        let mut f = Fixture { id, name, fixture_type: t.id.clone(), mode: mode.name.clone(), universe, address, position: pos, rotation: rot, ..Default::default() };
        if mode.breaks() > 1 {
            // The pixel ring is on its own universe.
            let at = self.l.next_free_address(4, mode.footprint_of(1)).expect("universe 4 full");
            f.breaks = vec![DmxAddress { universe: 4, address: at }];
        }
        self.l.fixtures.push(f);
        id
    }

    fn patch(&mut self) {
        let generic = |id: &str| openq::light::library::generic(id).expect("generic type");
        let bar = |id, name: &str, a: [f32; 2], b: [f32; 2], h| PlotShape { id, kind: ShapeKind::Bar, name: name.into(), a, b, height: h, label: [0.0, 0.0] };
        self.l.shapes = vec![
            PlotShape { id: 1, kind: ShapeKind::Rect, name: "Stage".into(), a: [-6.0, -4.0], b: [6.0, 4.0], height: 0.0, label: [0.0, 0.0] },
            PlotShape { id: 2, kind: ShapeKind::Rect, name: "Lighthouse riser".into(), a: [2.0, -3.5], b: [4.5, -1.5], height: 1.2, label: [0.0, 0.0] },
            bar(3, "FOH bridge", [-6.0, 9.0], [6.0, 9.0], 8.0),
            bar(4, "LX1", [-6.0, 2.0], [6.0, 2.0], 7.0),
            bar(5, "LX2", [-6.0, -0.5], [6.0, -0.5], 7.0),
            bar(6, "LX3", [-6.0, -3.0], [6.0, -3.0], 7.0),
            bar(7, "Cyc bar", [-6.0, -4.5], [6.0, -4.5], 7.5),
            bar(8, "Cyc floor", [-6.0, -4.2], [6.0, -4.2], 0.2),
            bar(9, "Ladder SL", [-7.0, -3.0], [-7.0, 3.0], 4.0),
            bar(10, "Ladder SR", [7.0, -3.0], [7.0, 3.0], 4.0),
            bar(11, "Auditorium", [-8.0, 14.0], [8.0, 14.0], 9.0),
        ];
        // Plot editor: an added text and a re-worded audience line.
        self.l.plot.notes.push(PlotNote { id: 1, at: [-5.5, 12.0], text: "FOH control\nrow M".into(), bold: true, colour: [160, 30, 30], ..Default::default() });
        self.l.plot.edit_mut(&PlotItem::Audience).text = Some("AUDIENCE · 420 seats".into());
        let foh = self.l.plot.edit_mut(&PlotItem::Shape(3));
        foh.size = Some(14.0);
        foh.colour = Some([30, 70, 160]);
        // Plot symbols: the first FOH fixture drawn as a profile, the RGBAW+UV type as PARs.
        self.l.plot.edit_mut(&PlotItem::Fixture(1)).symbol = Some(PlotSymbol::Profile);
        self.l.plot.edit_mut(&PlotItem::LegendRow("Generic RGBAW+UV".into())).symbol = Some(PlotSymbol::Par);
        let shape = |s: &[PlotShape], name: &str| s.iter().find(|x| x.name == name).expect("shape").clone();
        let shapes = self.l.shapes.clone();
        // No rotation = hanging with the beam down (GDTF/MVR), moving heads included.
        let down = [0.0, 0.0, 0.0];

        let dimmer = generic("generic/dimmer");
        for (i, p) in shape(&shapes, "FOH bridge").hang(12).into_iter().enumerate() {
            let id = self.add(1 + i as u32, format!("FOH {}", i + 1), &dimmer, 1, p, down);
            self.foh.push(id);
        }
        let drgbw = generic("generic/drgbw");
        for (k, bar) in ["LX1", "LX2"].iter().enumerate() {
            for (i, p) in shape(&shapes, bar).hang(8).into_iter().enumerate() {
                let n = k * 8 + i;
                let id = self.add(21 + n as u32, format!("{bar} wash {}", i + 1), &drgbw, 1, p, down);
                self.over.push(id);
            }
        }
        let rgbaw = generic("generic/rgbaw-uv");
        for (i, p) in shape(&shapes, "Cyc floor").hang(8).into_iter().enumerate() {
            let id = self.add(41 + i as u32, format!("Cyc {}", i + 1), &rgbaw, 1, p, [-60.0, 0.0, 0.0]);
            self.cyc.push(id);
        }
        let strobe = generic("generic/strobe");
        for (i, p) in shape(&shapes, "LX3").hang(4).into_iter().enumerate() {
            let id = self.add(401 + i as u32, format!("Lightning {}", i + 1), &strobe, 1, p, down);
            self.strobes.push(id);
        }
        let fog = generic("generic/fog");
        self.fog = self.add(501, "Smoke machine USL".into(), &fog, 1, [-5.5, 0.0, -3.5], down);
        self.haze = self.add(502, "Hazer SR".into(), &fog, 1, [6.5, 0.0, 1.0], down);
        let cmy = generic("generic/cmy");
        for (i, p) in shape(&shapes, "Auditorium").hang(2).into_iter().enumerate() {
            let id = self.add(601 + i as u32, format!("Followspot {}", i + 1), &cmy, 1, p, down);
            self.follow.push(id);
        }
        let mut house = generic("generic/dimmer");
        house.modes.reverse(); // the 16-bit mode first: patched in it
        house.id = "generic/dimmer-16".into();
        house.name = "Dimmer (16 bit)".into();
        for i in 0..4 {
            let id = self.add(901 + i, format!("House {}", i + 1), &house, 1, [-6.0 + 4.0 * i as f32, 10.0, 10.0], down);
            self.house.push(id);
        }

        let spot = profile_type();
        for (k, bar) in ["LX1", "LX3"].iter().enumerate() {
            for (i, p) in shape(&shapes, bar).hang(3).into_iter().enumerate() {
                let n = (k * 3 + i) as u32;
                let id = self.add(101 + n, format!("Spot {}", n + 1), &spot, 2, p, down);
                self.spots.push(id);
            }
        }
        let wash = generic("generic/wash");
        for (i, p) in shape(&shapes, "LX2").hang(8).into_iter().enumerate() {
            let id = self.add(111 + i as u32, format!("Wash {}", i + 1), &wash, 2, p, down);
            self.washes.push(id);
        }
        let batten = batten_type();
        for (i, p) in shape(&shapes, "Cyc bar").hang(4).into_iter().enumerate() {
            let id = self.add(201 + i as u32, format!("Pixel batten {}", i + 1), &batten, 3, p, [-90.0, 0.0, 0.0]);
            self.battens.push(id);
            self.cells.extend((1..=8).map(|c| cell_id(id, c)));
        }
        let hybrid = hybrid_type();
        for (i, (bar, x)) in [("Ladder SL", -7.0), ("Ladder SR", 7.0)].iter().enumerate() {
            let _ = bar;
            let id = self.add(301 + i as u32, format!("Beam hybrid {}", if i == 0 { "SL" } else { "SR" }), &hybrid, 3, [*x, 4.0, 0.0], [0.0, if i == 0 { 90.0 } else { -90.0 }, 0.0]);
            self.beams.push(id);
        }
        // Stage-right fixtures mirror stage left: their pan runs the other way.
        let right: Vec<FixtureId> = self.spots.iter().chain(&self.washes).copied().collect();
        for f in self.l.fixtures.iter_mut() {
            if right.contains(&f.id) && f.position[0] > 0.0 {
                f.invert_pan = true;
            }
            if f.id == 302 {
                f.invert_tilt = true;
            }
        }
    }

    fn groups(&mut self) {
        let g = |id, name: &str, fixtures: &[FixtureId]| Group { id, name: name.into(), fixtures: fixtures.to_vec() };
        let spots_sl: Vec<FixtureId> = self.spots.iter().copied().filter(|f| f % 2 == 1).collect();
        let odd_cells: Vec<FixtureId> = self.cells.iter().copied().step_by(2).collect();
        self.l.groups = vec![
            g(1, "FOH", &self.foh),
            g(2, "Overhead wash", &self.over),
            g(3, "Cyc", &self.cyc),
            g(4, "Spots", &self.spots),
            g(5, "Washes", &self.washes),
            g(6, "Pixel battens", &self.battens),
            g(7, "Batten cells", &self.cells),
            g(8, "Lightning", &self.strobes),
            g(9, "Atmospherics", &[self.fog, self.haze]),
            g(10, "House", &self.house),
            g(11, "Beam hybrids", &self.beams),
            g(12, "Followspots", &self.follow),
            g(13, "Spots (odd)", &spots_sl),
            g(14, "Batten cells (odd)", &odd_cells),
        ];
    }

    /// Focus points: the "Specials up" cues aim the spots at DSL/DSR through them.
    fn points(&mut self) {
        let pt = |id, name: &str, position| FocusPoint { id, name: name.into(), position };
        self.l.points = vec![
            pt(PT_CENTRE, "Centre", [0.0, 0.0, 0.0]),
            pt(PT_DSL, "DSL", [-3.5, 0.0, 2.5]),
            pt(PT_DSR, "DSR", [3.5, 0.0, 2.5]),
            pt(PT_RISER, "Lighthouse riser", [3.25, 1.2, -2.5]),
        ];
    }

    fn presets(&mut self) {
        let spot = self.spots[0];
        let wash = self.washes[0];
        let over = self.over[0];
        let cyc = self.cyc[0];
        let follow = self.follow[0];
        let beam = self.beams[0];
        let colour = |rgbw: [f32; 4], wheel: &str, cmy: [f32; 3]| -> Vec<(FixtureId, Vec<(A, f32)>)> {
            let [r, g, b, w] = rgbw;
            let mut v = vec![
                (over, vec![(A::Red, r), (A::Green, g), (A::Blue, b), (A::White, w)]),
                (wash, vec![(A::Red, r), (A::Green, g), (A::Blue, b), (A::White, w)]),
                (cyc, vec![(A::Red, r), (A::Green, g), (A::Blue, b), (A::Amber, (r * 0.6).min(1.0)), (A::White, w), (A::Uv, 0.0)]),
                (spot, vec![(A::ColorWheel, wheel_value(wheel)), (A::Cyan, cmy[0]), (A::Magenta, cmy[1]), (A::Yellow, cmy[2])]),
                (follow, vec![(A::Cyan, cmy[0]), (A::Magenta, cmy[1]), (A::Yellow, cmy[2])]),
                (beam, vec![(A::ColorWheel, wheel_value(wheel))]),
            ];
            for c in 1..=8 {
                v.push((cell_id(self.battens[0], c), vec![(A::Red, r), (A::Green, g), (A::Blue, b), (A::White, w)]));
            }
            for c in 1..=12 {
                v.push((cell_id(beam, c), vec![(A::Red, r), (A::Green, g), (A::Blue, b)]));
            }
            v
        };
        let pos = |pan: f32, tilt: f32| vec![(spot, vec![(A::Pan, pan), (A::Tilt, tilt)]), (wash, vec![(A::Pan, pan), (A::Tilt, tilt)]), (beam, vec![(A::Pan, pan), (A::Tilt, tilt)])];
        let beam_p = |gobo: &str, zoom: f32, iris: f32, focus: f32| vec![(spot, vec![(A::Gobo, gobo_value(gobo)), (A::Zoom, zoom), (A::Iris, iris), (A::Focus, focus)])];
        let mk = |id, name: &str, feature, v: Vec<(FixtureId, Vec<(A, f32)>)>| Preset {
            id,
            name: name.into(),
            feature,
            values: v
                .into_iter()
                .fold(BTreeMap::<FixtureId, BTreeMap<A, f32>>::new(), |mut m, (f, vals)| {
                    m.entry(f).or_default().extend(vals);
                    m
                })
                .into(),
        };
        let mut full = colour([1.0, 0.85, 0.7, 1.0], "Open", [0.0, 0.05, 0.15]);
        full.extend(pos(0.5, 0.62));
        for (f, v) in &mut full {
            if *f == spot || *f == wash {
                v.push((A::Dimmer, 1.0));
            }
        }
        self.l.presets = vec![
            mk(P_WARM, "Warm white", Some(Feature::Colour), colour([1.0, 0.75, 0.45, 0.8], "Amber", [0.0, 0.15, 0.35])),
            mk(P_MOON, "Cool moon", Some(Feature::Colour), colour([0.35, 0.5, 1.0, 0.3], "CTB", [0.45, 0.2, 0.0])),
            mk(P_STORM, "Storm blue", Some(Feature::Colour), colour([0.05, 0.2, 1.0, 0.0], "Blue", [0.9, 0.5, 0.0])),
            mk(P_SUNSET, "Sunset amber", Some(Feature::Colour), colour([1.0, 0.45, 0.1, 0.1], "Amber", [0.0, 0.5, 0.9])),
            mk(P_RED, "Deep red", Some(Feature::Colour), colour([1.0, 0.0, 0.05, 0.0], "Red", [0.0, 1.0, 1.0])),
            mk(P_SEA, "Sea green", Some(Feature::Colour), colour([0.0, 0.7, 0.55, 0.0], "Green", [0.8, 0.0, 0.5])),
            mk(P_LAVENDER, "Lavender", Some(Feature::Colour), colour([0.6, 0.4, 1.0, 0.2], "Magenta", [0.3, 0.5, 0.0])),
            mk(P_CENTRE, "Centre stage", Some(Feature::Position), pos(0.5, 0.62)),
            mk(P_DSL, "DSL special", Some(Feature::Position), pos(0.42, 0.7)),
            mk(P_DSR, "DSR special", Some(Feature::Position), pos(0.58, 0.7)),
            mk(P_DOOR, "Door USR", Some(Feature::Position), pos(0.62, 0.55)),
            mk(P_AUDIENCE, "Audience sweep", Some(Feature::Position), pos(0.5, 0.9)),
            mk(P_LAMP, "Lighthouse lamp", Some(Feature::Position), pos(0.6, 0.45)),
            mk(P_WINDOW, "Gobo window", Some(Feature::Beam), beam_p("Window", 0.4, 0.0, 0.55)),
            mk(P_LEAVES, "Gobo leaves", Some(Feature::Beam), beam_p("Leaves", 0.7, 0.0, 0.4)),
            mk(P_TIGHT, "Tight special", Some(Feature::Beam), beam_p("Open", 0.1, 0.65, 0.5)),
            mk(P_FLOOD, "Wide flood", Some(Feature::Beam), beam_p("Open", 1.0, 0.0, 0.5)),
            mk(P_FULL, "Full stage (all)", None, full),
        ];
    }

    /// Playback buttons: atmospherics (stored values), a strobe flash, presets on groups.
    fn buttons(&mut self) {
        let group = |l: &Lighting, id: u32| l.groups.iter().find(|g| g.id == id).map(|g| g.fixtures.clone()).unwrap_or_default();
        let values = |v: &[(FixtureId, A, f32)]| {
            let mut look = Look::new();
            for (f, a, x) in v {
                look.entry(*f).or_default().insert(*a, (*x).into());
            }
            look.into()
        };
        self.l.buttons = vec![
            LightButton { id: 1, name: "Fog".into(), color: [0x8a, 0x9b, 0xb0], values: values(&[(self.fog, A::Fog, 0.8)]), fade: 2.0, ..Default::default() },
            LightButton { id: 2, name: "Haze".into(), color: [0x6c, 0x7a, 0x8c], values: values(&[(self.haze, A::Fog, 0.5), (self.haze, A::Fan, 0.5)]), fade: 3.0, ..Default::default() },
            LightButton {
                id: 3,
                name: "Strobe hit".into(),
                color: [0xf0, 0xf0, 0xf0],
                values: values(&self.strobes.iter().map(|f| (*f, A::Dimmer, 1.0)).collect::<Vec<_>>()),
                effects: vec![EffectRun { effect: FX_LIGHTNING, fixtures: self.strobes.clone(), stop: false }],
                flash: true,
                ..Default::default()
            },
            LightButton { id: 4, name: "Cyc lavender".into(), color: [0x9a, 0x6a, 0xf0], preset: Some(P_LAVENDER), fixtures: group(&self.l, 3), fade: 1.5, ..Default::default() },
            LightButton { id: 5, name: "Warm wash".into(), color: [0xf0, 0xa0, 0x50], preset: Some(P_WARM), fixtures: group(&self.l, 5), fade: 1.0, page: 1, ..Default::default() },
        ];
        self.l.button_pages = vec!["Atmos".into(), "Washes".into()];
    }

    fn effects(&mut self) {
        let e = |id, name: &str, attribute, shape| Effect { id, name: name.into(), attribute, shape, ..Default::default() };
        let mut candle = e(FX_CANDLE, "Candle flicker", A::Dimmer, Shape::Random);
        candle.speed = 6.0;
        candle.size = 0.25;
        candle.relative = true;
        candle.spread = 0.0;
        let mut waves = e(FX_WAVES, "Waves on the cyc", A::Blue, Shape::Sine);
        waves.speed = 0.15;
        waves.size = 0.4;
        waves.relative = true;
        waves.spread = 360.0;
        waves.wings = 2;
        let mut lightning = e(FX_LIGHTNING, "Lightning", A::Dimmer, Shape::Square);
        lightning.speed = 0.7;
        lightning.duty = 0.06;
        lightning.size = 1.0;
        lightning.spread = 90.0;
        let mut sweep = e(FX_SWEEP, "Lighthouse sweep", A::Pan, Shape::Circle);
        sweep.speed = 0.08;
        sweep.size = 0.35;
        sweep.relative = true;
        sweep.spread = 0.0;
        let mut chase = e(FX_CHASE, "Batten chase (tempo)", A::Dimmer, Shape::Square);
        chase.sync = true;
        chase.speed = 0.5;
        chase.duty = 0.25;
        chase.groups = 2;
        chase.wings = 2;
        let mut pulse = e(FX_PULSE, "Pulse with the music", A::Dimmer, Shape::Sine);
        pulse.speed = 1.0;
        pulse.size = 0.6;
        pulse.relative = true;
        pulse.audio = Some(AudioReact { gain: 3.0, floor: 0.15 });
        let mut haze = e(FX_HAZE, "Haze breathing", A::Fog, Shape::Triangle);
        haze.speed = 0.02;
        haze.size = 0.3;
        haze.base = 0.1;
        let mut ramp = e(FX_RAMP, "Followspot shimmer", A::Dimmer, Shape::SawDown);
        ramp.speed = 2.0;
        ramp.size = 0.15;
        ramp.relative = true;
        let mut ramp_up = e(10, "Strobe build", A::Shutter, Shape::SawUp);
        ramp_up.speed = 0.25;
        ramp_up.size = 0.5;
        ramp_up.base = 0.1;
        self.l.effects = vec![candle, waves, lightning, sweep, chase, pulse, rainbow(), haze, ramp, ramp_up];
    }

    fn sequences(&mut self) {
        let mut main = Sequence { id: 1, name: "Main".into(), move_in_black: true, mib_delay: 1.0, mib_fade: 3.0, ..Default::default() };
        let others = [7usize, 20, 8, 6];
        let main_count = LIGHT_CUES - others.iter().sum::<usize>();
        let moods = scene_moods();
        let per_scene: Vec<usize> = {
            let n = moods.len();
            (0..n).map(|i| main_count / n + usize::from(i < main_count % n)).collect()
        };
        let mut num = 0;
        let mut seed = 7u32;
        for (s, (mood, title)) in moods.iter().enumerate() {
            for k in 0..per_scene[s] {
                num += 1;
                seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                let c = self.main_cue(num, s, k, *mood, title, seed);
                main.cues.push(c);
            }
        }
        // Each act's first look is blocked: later edits to Act One don't leak into Act Two.
        let act2 = per_scene.iter().take(ACT1.len()).sum::<usize>();
        main.block(0);
        main.block(act2);
        main.cues[act2].name = format!("{} (block)", main.cues[act2].name);

        let mut house = Sequence { id: 2, name: "House & preshow".into(), ..Default::default() };
        // In the order the lists fire them.
        let house_looks: [(&str, f32, f32, f64); 7] = [
            ("House out (Act One)", 0.0, 0.0, 4.0),
            ("House full, curtain warmers", 1.0, 0.45, 3.0),
            ("Beginners: house to half", 0.5, 0.3, 6.0),
            ("Interval: house up", 0.9, 0.25, 6.0),
            ("Beginners: house to half", 0.5, 0.25, 6.0),
            ("House out (Act Two)", 0.0, 0.0, 4.0),
            ("House up", 1.0, 0.0, 5.0),
        ];
        for (i, (name, h, warm, fade)) in house_looks.iter().enumerate() {
            let mut c = LightCue { number: (i + 1).to_string(), name: name.to_string(), fade: *fade, ..Default::default() };
            for f in &self.house {
                set(&mut c, *f, A::Dimmer, *h);
            }
            for f in &self.foh {
                set(&mut c, *f, A::Dimmer, *warm);
            }
            if i == 5 {
                c.down_fade = Some(6.0);
            }
            house.cues.push(c);
        }

        let mut storm = Sequence { id: 3, name: "Storm FX".into(), ..Default::default() };
        for i in 0..20 {
            let mut c = LightCue { number: (i + 1).to_string(), name: format!("Flash {}", i + 1), fade: 0.0, down_fade: Some(0.6 + (i % 3) as f64 * 0.3), cue_only: true, ..Default::default() };
            for (k, f) in self.strobes.iter().enumerate() {
                if (i + k) % 3 != 0 {
                    set(&mut c, *f, A::Dimmer, 1.0);
                    set(&mut c, *f, A::Shutter, 0.2);
                }
            }
            for f in &self.cyc {
                set(&mut c, *f, A::White, 1.0);
            }
            match i {
                0 => {
                    c.name = "Storm builds (lightning effect)".into();
                    c.cue_only = false;
                    c.fade = 3.0;
                    c.effects.push(EffectRun { effect: FX_LIGHTNING, fixtures: self.strobes.clone(), stop: false });
                    c.effects.push(EffectRun { effect: 10, fixtures: self.strobes.clone(), stop: false });
                    set(&mut c, self.haze, A::Fog, 0.6);
                    set(&mut c, self.haze, A::Fan, 0.5);
                    c.effects.push(EffectRun { effect: FX_HAZE, fixtures: vec![self.haze], stop: false });
                }
                19 => {
                    c.name = "Storm passes".into();
                    c.cue_only = false;
                    c.fade = 4.0;
                    c.effects.push(EffectRun { effect: FX_LIGHTNING, fixtures: self.strobes.clone(), stop: true });
                    c.effects.push(EffectRun { effect: 10, fixtures: self.strobes.clone(), stop: true });
                    for f in &self.strobes {
                        set(&mut c, *f, A::Dimmer, 0.0);
                    }
                }
                _ => {}
            }
            storm.cues.push(c);
        }

        let mut lamp = Sequence { id: 4, name: "Lighthouse lamp".into(), ..Default::default() };
        let lamp_looks = ["Lamp lit", "Lamp sweeps", "Lamp at rest", "Lamp gutters", "Lamp dies (snap)", "Lamp relit", "Lamp full beam", "Lamp sweeps (finale)"];
        for (i, name) in lamp_looks.iter().enumerate() {
            let mut c = LightCue { number: (i + 1).to_string(), name: name.to_string(), fade: 4.0, delay: if i == 1 { 1.0 } else { 0.0 }, ..Default::default() };
            let level = match i {
                4 => 0.0,
                3 => 0.4,
                _ => 1.0,
            };
            for f in &self.beams {
                set(&mut c, *f, A::Dimmer, level);
                setp(&mut c, *f, A::Pan, P_LAMP);
                setp(&mut c, *f, A::Tilt, P_LAMP);
                set(&mut c, *f, A::ColorWheel, wheel_value("Amber"));
                set(&mut c, *f, A::Prism, if i == 6 { 0.5 } else { 0.0 });
            }
            for (k, cell) in self.beams.iter().flat_map(|b| (1..=12).map(move |c| cell_id(*b, c))).enumerate() {
                set(&mut c, cell, A::Red, level);
                set(&mut c, cell, A::Green, level * if k % 2 == 0 { 0.7 } else { 0.5 });
                set(&mut c, cell, A::Blue, 0.0);
            }
            if i == 4 {
                c.fade = 0.0;
            }
            if matches!(i, 1 | 7) {
                c.effects.push(EffectRun { effect: FX_SWEEP, fixtures: self.beams.clone(), stop: false });
                c.feature_times.insert(Feature::Position, FeatureTime { fade: 6.0, delay: 0.5 });
            }
            if matches!(i, 2 | 4) {
                c.effects.push(EffectRun { effect: FX_SWEEP, fixtures: self.beams.clone(), stop: true });
            }
            lamp.cues.push(c);
        }

        let mut bows = Sequence { id: 5, name: "Curtain call".into(), ..Default::default() };
        for (i, name) in ["Company bow", "Principals", "Keeper's bow", "Band", "Full company", "Out"].iter().enumerate() {
            let mut c = LightCue { number: (i + 1).to_string(), name: name.to_string(), fade: if i == 5 { 6.0 } else { 1.5 }, ..Default::default() };
            let level = if i == 5 { 0.0 } else { 1.0 };
            for f in self.foh.iter().chain(&self.over) {
                set(&mut c, *f, A::Dimmer, level);
            }
            for f in &self.over {
                setp(&mut c, *f, A::Red, P_WARM);
                setp(&mut c, *f, A::Green, P_WARM);
                setp(&mut c, *f, A::Blue, P_WARM);
                setp(&mut c, *f, A::White, P_WARM);
            }
            for f in &self.follow {
                set(&mut c, *f, A::Dimmer, if i == 2 { 1.0 } else { 0.0 });
            }
            if i == 0 {
                c.effects.push(EffectRun { effect: FX_PULSE, fixtures: self.washes.clone(), stop: false });
                c.effects.push(EffectRun { effect: FX_RAINBOW, fixtures: self.cells.clone(), stop: false });
                for f in &self.washes {
                    set(&mut c, *f, A::Dimmer, 0.7);
                }
                for cell in &self.cells {
                    set(&mut c, *cell, A::Red, 0.5);
                }
                for f in &self.battens {
                    set(&mut c, *f, A::Dimmer, 1.0);
                }
            }
            if i == 2 {
                c.effects.push(EffectRun { effect: FX_RAMP, fixtures: self.follow.clone(), stop: false });
            }
            if i == 5 {
                for f in self.washes.iter().chain(&self.battens) {
                    set(&mut c, *f, A::Dimmer, 0.0);
                }
                c.effects.push(EffectRun { effect: FX_PULSE, fixtures: self.washes.clone(), stop: true });
                c.effects.push(EffectRun { effect: FX_RAINBOW, fixtures: self.cells.clone(), stop: true });
            }
            bows.cues.push(c);
        }
        self.l.sequences = vec![main, house, storm, lamp, bows];
        debug_assert_eq!(others[0], self.l.sequences[1].cues.len());
    }

    /// Cue `k` of scene `s` in the main sequence.
    fn main_cue(&self, num: usize, s: usize, k: usize, mood: Mood, title: &str, seed: u32) -> LightCue {
        let mut c = LightCue { number: num.to_string(), ..Default::default() };
        let jitter = |n: u32| (seed.rotate_right(n % 32) % 100) as f32 / 100.0;
        let (colour, cyc_rgb, foh, gobo, pos) = look(mood);
        match k {
            0 => {
                c.name = format!("{title}: scene look");
                c.fade = if mood == Mood::Storm { 2.0 } else { 6.0 };
                c.down_fade = Some(c.fade + 2.0);
                c.feature_times.insert(Feature::Position, FeatureTime { fade: 3.0, delay: 1.0 });
                c.feature_times.insert(Feature::Colour, FeatureTime { fade: c.fade + 1.0, delay: 0.0 });
                for (i, f) in self.foh.iter().enumerate() {
                    set(&mut c, *f, A::Dimmer, (foh * (0.8 + 0.4 * ((i % 3) as f32 / 2.0))).min(1.0));
                }
                for f in &self.over {
                    set(&mut c, *f, A::Dimmer, foh * 0.9);
                    for a in [A::Red, A::Green, A::Blue, A::White] {
                        setp(&mut c, *f, a, colour);
                    }
                }
                for (i, f) in self.cyc.iter().enumerate() {
                    let g = 1.0 - (i as f32 / 8.0 - 0.5).abs() * 0.4;
                    set(&mut c, *f, A::Red, cyc_rgb[0] * g);
                    set(&mut c, *f, A::Green, cyc_rgb[1] * g);
                    set(&mut c, *f, A::Blue, cyc_rgb[2] * g);
                    set(&mut c, *f, A::Uv, if mood == Mood::Dark || mood == Mood::Storm { 0.6 } else { 0.0 });
                    set(&mut c, *f, A::Dimmer, 0.9);
                }
                for f in &self.washes {
                    set(&mut c, *f, A::Dimmer, 0.55);
                    for a in [A::Red, A::Green, A::Blue, A::White] {
                        setp(&mut c, *f, a, colour);
                    }
                    setp(&mut c, *f, A::Pan, pos);
                    setp(&mut c, *f, A::Tilt, pos);
                    set(&mut c, *f, A::Zoom, 0.6);
                }
                for f in &self.spots {
                    set(&mut c, *f, A::Dimmer, 0.0);
                }
                for f in &self.battens {
                    set(&mut c, *f, A::Dimmer, if matches!(mood, Mood::Village | Mood::Finale | Mood::Dawn) { 0.8 } else { 0.3 });
                }
                for (i, cell) in self.cells.iter().enumerate() {
                    let t = (i % 8) as f32 / 7.0;
                    set(&mut c, *cell, A::Red, cyc_rgb[0] * (1.0 - t * 0.5));
                    set(&mut c, *cell, A::Green, cyc_rgb[1]);
                    set(&mut c, *cell, A::Blue, cyc_rgb[2] * (0.5 + t * 0.5));
                    set(&mut c, *cell, A::White, 0.0);
                }
                // Effects that belong to the mood start here and stop in the next scene's look.
                let all_fx = [FX_CANDLE, FX_WAVES, FX_SWEEP, FX_CHASE];
                let mine: &[u32] = match mood {
                    Mood::Interior | Mood::Inn => &[FX_CANDLE],
                    Mood::Storm | Mood::Dark => &[FX_WAVES],
                    Mood::Night => &[FX_SWEEP],
                    Mood::Village | Mood::Finale => &[FX_CHASE],
                    _ => &[],
                };
                for fx in all_fx {
                    let fixtures = match fx {
                        FX_CANDLE => self.over.clone(),
                        FX_WAVES => self.cyc.clone(),
                        FX_SWEEP => self.spots.clone(),
                        _ => self.battens.clone(),
                    };
                    c.effects.push(EffectRun { effect: fx, fixtures, stop: !mine.contains(&fx) });
                }
                if s == 0 {
                    c.name = format!("{title}: opening look");
                }
            }
            1 => {
                c.name = "Specials up".into();
                c.fade = 3.0;
                for (i, f) in self.spots.iter().enumerate() {
                    set(&mut c, *f, A::Dimmer, if i < 4 { 0.85 } else { 0.0 });
                    let point = if i % 2 == 0 { PT_DSL } else { PT_DSR };
                    for a in [A::Pan, A::Tilt] {
                        c.values.entry(*f).or_default().insert(a, Value::point(point));
                    }
                    setp(&mut c, *f, A::Gobo, gobo);
                    setp(&mut c, *f, A::Zoom, gobo);
                    setp(&mut c, *f, A::Iris, gobo);
                    setp(&mut c, *f, A::Focus, gobo);
                    for a in [A::ColorWheel, A::Cyan, A::Magenta, A::Yellow] {
                        setp(&mut c, *f, a, colour);
                    }
                }
            }
            2 => {
                c.name = "Cyc shifts".into();
                c.fade = 8.0;
                c.delay = 1.0;
                for (i, f) in self.cyc.iter().enumerate() {
                    set(&mut c, *f, A::Blue, (cyc_rgb[2] + 0.2 * jitter(i as u32)).min(1.0));
                    set(&mut c, *f, A::Amber, 0.3 * jitter(3 + i as u32));
                }
            }
            3 => {
                c.name = "Isolate centre".into();
                c.fade = 3.0;
                c.down_fade = Some(5.0);
                for (i, f) in self.foh.iter().enumerate() {
                    set(&mut c, *f, A::Dimmer, if (4..8).contains(&i) { foh } else { foh * 0.3 });
                }
                for f in &self.follow {
                    set(&mut c, *f, A::Dimmer, 0.8);
                    set(&mut c, *f, A::Yellow, 0.2);
                }
            }
            4 => {
                c.name = "Door special".into();
                c.fade = 2.0;
                if let Some(f) = self.spots.last() {
                    set(&mut c, *f, A::Dimmer, 0.9);
                    setp(&mut c, *f, A::Pan, P_DOOR);
                    setp(&mut c, *f, A::Tilt, P_DOOR);
                    setp(&mut c, *f, A::Gobo, P_TIGHT);
                    set(&mut c, *f, A::Shutter, shutter_open());
                }
                for f in &self.follow {
                    set(&mut c, *f, A::Dimmer, 0.0);
                }
            }
            5 if mood == Mood::Storm || mood == Mood::Dark => {
                c.name = "Snap to dark".into();
                c.fade = 0.0;
                c.cue_only = true;
                for f in self.foh.iter().chain(&self.over) {
                    set(&mut c, *f, A::Dimmer, 0.05);
                }
            }
            5 => {
                c.name = "Warm up".into();
                c.fade = 10.0;
                for f in &self.over {
                    set(&mut c, *f, A::Dimmer, (foh + 0.1).min(1.0));
                    set(&mut c, *f, A::White, 0.7);
                }
            }
            6 => {
                c.name = "Washes move".into();
                c.fade = 4.0;
                c.feature_times.insert(Feature::Position, FeatureTime { fade: 5.0, delay: 0.0 });
                for (i, f) in self.washes.iter().enumerate() {
                    set(&mut c, *f, A::Pan, 0.4 + 0.2 * jitter(i as u32));
                    set(&mut c, *f, A::Tilt, 0.55 + 0.1 * jitter(5 + i as u32));
                    set(&mut c, *f, A::Dimmer, 0.7);
                }
            }
            7 => {
                c.name = "Battens accent".into();
                c.fade = 2.5;
                for (i, cell) in self.cells.iter().enumerate() {
                    set(&mut c, *cell, A::White, if i % 4 == 0 { 0.6 } else { 0.0 });
                }
            }
            8 => {
                c.name = "Specials out".into();
                c.fade = 4.0;
                for f in &self.spots {
                    set(&mut c, *f, A::Dimmer, 0.0);
                }
            }
            9 => {
                c.name = "Front fill".into();
                c.fade = 3.0;
                for f in &self.foh {
                    set(&mut c, *f, A::Dimmer, (foh + 0.15).min(1.0));
                }
            }
            10 => {
                c.name = "Breath".into();
                c.fade = 6.0;
                for f in &self.over {
                    set(&mut c, *f, A::Dimmer, foh * (0.6 + 0.3 * jitter(*f)));
                }
            }
            _ => {
                c.name = "Button".into();
                c.fade = 1.0;
                c.delay = 0.5;
                for f in self.foh.iter().chain(&self.over).chain(&self.washes) {
                    set(&mut c, *f, A::Dimmer, foh * 0.4);
                }
            }
        }
        c
    }
}

/// (colour preset, cyc colour, front light level, beam preset, position preset) of a mood.
fn look(mood: Mood) -> (u32, [f32; 3], f32, u32, u32) {
    match mood {
        Mood::Storm => (P_STORM, [0.05, 0.1, 0.6], 0.35, P_LEAVES, P_CENTRE),
        Mood::Dawn => (P_SUNSET, [1.0, 0.45, 0.2], 0.7, P_FLOOD, P_CENTRE),
        Mood::Interior => (P_WARM, [0.2, 0.1, 0.05], 0.85, P_WINDOW, P_DSL),
        Mood::Night => (P_MOON, [0.0, 0.05, 0.35], 0.4, P_LEAVES, P_DSR),
        Mood::Village => (P_WARM, [0.3, 0.6, 1.0], 0.9, P_FLOOD, P_CENTRE),
        Mood::Inn => (P_SUNSET, [0.3, 0.12, 0.02], 0.8, P_WINDOW, P_DSL),
        Mood::Memory => (P_LAVENDER, [0.5, 0.2, 0.7], 0.55, P_LEAVES, P_DSR),
        Mood::Dark => (P_SEA, [0.0, 0.12, 0.18], 0.2, P_TIGHT, P_CENTRE),
        Mood::Finale => (P_RED, [1.0, 0.3, 0.1], 1.0, P_FLOOD, P_AUDIENCE),
    }
}

fn texture_pool(mood: Mood) -> &'static [&'static str] {
    match mood {
        Mood::Storm => &["thunder_crack", "rolling_thunder", "wind", "strong_wind", "debris_hits", "low_thunder_rolls", "distant_thunder", "rain_heavy_loud"],
        Mood::Dawn => &["june_songbirds", "crow_call", "water_lapping_wind", "small_stream_flowing", "wind_chimes", "distant_dog_barking"],
        Mood::Interior => &["clock_ticking", "fireplace_tools", "cup_on_saucer", "glosing_old_window", "wind_quiet_creaks", "latch_opening_closing", "distant_footsteps_on_wood"],
        Mood::Night => &["owl_hooting", "afternoon_crickets_long", "wind", "distant_dog_barking", "wind_quiet_creaks"],
        Mood::Village => &["crowd_talking", "medium_bell_ringing_near", "dock_movement", "distant_dog_barking", "team_cheer", "voices_murmur"],
        Mood::Inn => &["cup_on_saucer", "murmuring_and_talking_men", "team_cheer", "door_close_and_lock", "broken_plates", "fireplace_tools"],
        Mood::Memory => &["wind_chimes", "rainstick_slow", "small_stream_flowing", "mechanical_clock_ring", "june_songbirds"],
        Mood::Dark => &["large_bubbles_surfacing", "air_woosh_underwater", "wind_quiet_creaks", "gas_lamp_flickering", "rainstick_slow"],
        Mood::Finale => &["team_cheer", "tympani_bing", "drum_roll", "waves_crashing_on_rock_beach", "medium_bell_ringing_near"],
    }
}

fn scene_moods() -> Vec<(Mood, &'static str)> {
    ACT1.iter().chain(ACT2).map(|s| (s.mood, s.title)).collect()
}

fn set(c: &mut LightCue, f: FixtureId, a: A, v: f32) {
    c.values.entry(f).or_default().insert(a, Value::Level(v.clamp(0.0, 1.0)));
}

fn setp(c: &mut LightCue, f: FixtureId, a: A, preset: u32) {
    c.values.entry(f).or_default().insert(a, Value::Preset { preset });
}

// ---------------------------------------------------------------------------------------------
// Fixture types

fn ch(attribute: A, offset: u16, fine: Option<u16>) -> ChannelDef {
    ChannelDef { attribute, offset, fine, ..Default::default() }
}

fn r(from: u8, to: u8, name: &str) -> ChannelRange {
    ChannelRange { from, to, name: name.into(), ..Default::default() }
}

const WHEEL: [(&str, [u8; 3]); 8] =
    [("Open", [255, 255, 255]), ("Red", [230, 20, 30]), ("Amber", [255, 150, 20]), ("Green", [20, 200, 60]), ("Blue", [20, 60, 255]), ("CTB", [170, 200, 255]), ("Magenta", [230, 30, 200]), ("UV", [90, 0, 200])];
const GOBOS: [&str; 6] = ["Open", "Window", "Leaves", "Dots", "Star", "Breakup"];

fn wheel_value(name: &str) -> f32 {
    let i = WHEEL.iter().position(|(n, _)| *n == name).unwrap_or(0) as f32;
    (i * 10.0 + 4.5) / 255.0
}

fn gobo_value(name: &str) -> f32 {
    let i = GOBOS.iter().position(|n| *n == name).unwrap_or(0) as f32;
    (i * 10.0 + 4.5) / 255.0
}

fn shutter_open() -> f32 {
    11.5 / 255.0
}

/// A profile spot with named wheel slots, gobo pictures and channels that have no attribute of
/// their own (Extra).
fn profile_type() -> FixtureType {
    let shutter = ChannelDef {
        ranges: vec![r(0, 7, "Closed"), r(8, 15, "Open"), r(16, 131, "Strobe slow → fast"), r(132, 139, "Open"), r(140, 189, "Pulse"), r(190, 239, "Random strobe"), r(240, 255, "Open")],
        ..ch(A::Shutter, 7, None)
    };
    let wheel = ChannelDef {
        ranges: WHEEL
            .iter()
            .enumerate()
            .map(|(i, (n, rgb))| ChannelRange { color: Some(*rgb), ..r(i as u8 * 10, i as u8 * 10 + 9, n) })
            .chain([r(128, 191, "Rotate slow → fast"), r(192, 255, "Random")])
            .collect(),
        ..ch(A::ColorWheel, 8, None)
    };
    let mut images = BTreeMap::new();
    let gobo = ChannelDef {
        ranges: GOBOS
            .iter()
            .enumerate()
            .map(|(i, n)| {
                let mut range = r(i as u8 * 10, i as u8 * 10 + 9, n);
                if i > 0 {
                    let file = format!("{}.svg", n.to_lowercase());
                    match openq::light::gobo::thumbnail(gobo_svg(n).as_bytes()) {
                        Ok(png) => {
                            images.insert(file.clone(), png);
                        }
                        Err(e) => eprintln!("gobo {n}: {e:#}"),
                    }
                    range.image = file;
                }
                range
            })
            .chain([r(64, 127, "Shake slow → fast"), r(128, 255, "Wheel spin")])
            .collect(),
        ..ch(A::Gobo, 13, None)
    };
    let gobo_rot = ChannelDef { ranges: vec![r(0, 127, "Index"), r(128, 189, "Rotate CW fast → slow"), r(190, 193, "Stop"), r(194, 255, "Rotate CCW slow → fast")], ..ch(A::GoboRotation, 14, None) };
    let prism = ChannelDef { ranges: vec![r(0, 19, "Open"), r(20, 127, "3-facet"), r(128, 255, "5-facet linear")], ..ch(A::Prism, 16, None) };
    let control = ChannelDef { ranges: vec![r(0, 9, "Normal"), r(10, 19, "Lamp on"), r(20, 29, "Lamp off"), r(30, 39, "Reset"), r(40, 255, "Normal")], ..ch(A::Control, 22, None) };
    let anim = ChannelDef { name: "Animation wheel".into(), ranges: vec![r(0, 9, "Out"), r(10, 127, "In, CW"), r(128, 255, "In, CCW")], ..ch(A::Extra(1), 23, None) };
    let fan = ChannelDef { name: "Fan mode".into(), ranges: vec![r(0, 127, "Auto"), r(128, 255, "Silent")], default: Some(0.75), ..ch(A::Extra(2), 24, None) };
    let channels = vec![
        ch(A::Pan, 0, Some(1)),
        ch(A::Tilt, 2, Some(3)),
        ch(A::PanTiltSpeed, 4, None),
        ChannelDef { highlight: Some(1.0), ..ch(A::Dimmer, 5, Some(6)) },
        shutter,
        wheel,
        ch(A::Cyan, 9, None),
        ch(A::Magenta, 10, None),
        ch(A::Yellow, 11, None),
        ch(A::Cto, 12, None),
        gobo,
        gobo_rot,
        ch(A::Gobo2, 15, None),
        prism,
        ch(A::PrismRotation, 17, None),
        ch(A::Frost, 18, None),
        ch(A::Iris, 19, None),
        ch(A::Zoom, 20, None),
        ch(A::Focus, 21, None),
        control,
        anim,
        fan,
    ];
    let extended = FixtureMode { name: "Extended (26 ch)".into(), channels, heads: Vec::new(), slots: 26 };
    let basic = FixtureMode::sequential("Basic (16 ch)", &[(A::Pan, true), (A::Tilt, true), (A::Dimmer, false), (A::Shutter, false), (A::ColorWheel, false), (A::Gobo, false), (A::Prism, false), (A::Focus, false), (A::Zoom, false), (A::Iris, false), (A::Control, false), (A::Frost, false)]);
    FixtureType {
        id: "openq-demo/profile-575".into(),
        manufacturer: "OpenQ Demo".into(),
        name: "Profile 575".into(),
        modes: vec![extended, basic],
        physical: Physical { pan_range: 540.0, tilt_range: 270.0, beam_min: 12.0, beam_max: 38.0, moving: true, ..Default::default() },
        images,
    }
}

/// An LED batten of 8 RGBW cells with a master dimmer and shutter.
fn batten_type() -> FixtureType {
    let heads = (0..8u16)
        .map(|i| Head {
            name: (i + 1).to_string(),
            channels: [A::Red, A::Green, A::Blue, A::White].iter().enumerate().map(|(k, a)| ch(*a, 2 + i * 4 + k as u16, None)).collect(),
        })
        .collect();
    let shutter = ChannelDef { ranges: vec![r(0, 9, "Open"), r(10, 250, "Strobe slow → fast"), r(251, 255, "Open")], default: Some(0.0), ..ch(A::Shutter, 1, None) };
    FixtureType {
        id: "openq-demo/pixel-batten-8".into(),
        manufacturer: "OpenQ Demo".into(),
        name: "Pixel batten 8".into(),
        modes: vec![FixtureMode { name: "8 cells + master (34 ch)".into(), channels: vec![ch(A::Dimmer, 0, None), shutter], heads, slots: 0 }],
        physical: Physical { beam_min: 30.0, beam_max: 30.0, moving: false, ..Default::default() },
        images: BTreeMap::new(),
    }
}

/// A beam light with a pixel ring on a second DMX input (two breaks).
fn hybrid_type() -> FixtureType {
    let mut m = FixtureMode::sequential(
        "Standard + pixel ring",
        &[(A::Pan, true), (A::Tilt, true), (A::Dimmer, false), (A::Shutter, false), (A::ColorWheel, false), (A::Gobo, false), (A::Prism, false), (A::Focus, false), (A::Zoom, false), (A::Control, false)],
    );
    m.heads = (0..12u16)
        .map(|i| Head {
            name: format!("Ring {}", i + 1),
            channels: [A::Red, A::Green, A::Blue].iter().enumerate().map(|(k, a)| ChannelDef { dmx_break: 1, ..ch(*a, i * 3 + k as u16, None) }).collect(),
        })
        .collect();
    FixtureType {
        id: "openq-demo/beam-hybrid".into(),
        manufacturer: "OpenQ Demo".into(),
        name: "Beam hybrid".into(),
        modes: vec![m],
        // Measured pixel colours (as a GDTF file gives them): the colour picker shows this gamut.
        physical: Physical {
            pan_range: 540.0,
            tilt_range: 270.0,
            beam_min: 2.0,
            beam_max: 20.0,
            moving: true,
            emitters: vec![
                openq::light::model::Emitter { attribute: A::Red, x: 0.692, y: 0.307, luminance: 0.25 },
                openq::light::model::Emitter { attribute: A::Green, x: 0.192, y: 0.718, luminance: 0.68 },
                openq::light::model::Emitter { attribute: A::Blue, x: 0.141, y: 0.046, luminance: 0.07 },
            ],
            ..Default::default()
        },
        images: BTreeMap::new(),
    }
}

fn gobo_svg(name: &str) -> String {
    let shapes = match name {
        "Window" => r#"<rect x="22" y="22" width="25" height="25"/><rect x="53" y="22" width="25" height="25"/><rect x="22" y="53" width="25" height="25"/><rect x="53" y="53" width="25" height="25"/>"#.to_string(),
        "Leaves" => (0..7)
            .map(|i| {
                let a = i as f32 * 0.9;
                format!(r#"<ellipse cx="{:.1}" cy="{:.1}" rx="14" ry="6" transform="rotate({:.0} {:.1} {:.1})"/>"#, 50.0 + 22.0 * a.cos(), 50.0 + 22.0 * a.sin(), a * 57.0, 50.0 + 22.0 * a.cos(), 50.0 + 22.0 * a.sin())
            })
            .collect(),
        "Dots" => (0..9).map(|i| format!(r#"<circle cx="{}" cy="{}" r="8"/>"#, 28 + (i % 3) * 22, 28 + (i / 3) * 22)).collect(),
        "Star" => r#"<polygon points="50,12 59,40 88,40 64,57 73,85 50,68 27,85 36,57 12,40 41,40"/>"#.to_string(),
        _ => (0..14).map(|i| format!(r#"<circle cx="{}" cy="{}" r="{}"/>"#, 20 + (i * 37) % 62, 22 + (i * 53) % 58, 4 + i % 5)).collect(),
    };
    format!(r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100"><circle cx="50" cy="50" r="48" fill="black"/><g fill="white">{shapes}</g></svg>"#)
}

/// The rainbow formula, worked out here as the Effects page would.
fn rainbow() -> Effect {
    const FORMULA: &str = r#"# A rainbow that runs along the pixel battens: every pattern row is a step further round the
# colour wheel. "sat" sets how pastel it is.
param sat 1 0..1
effect "Rainbow" steps 64 width 8 smooth
red, green, blue = hsv(t + row / width, sat)
"#;
    let out = openq::light::formula::evaluate(FORMULA, &[]).expect("rainbow formula");
    let table = out.effects[0].1.clone();
    Effect {
        id: FX_RAINBOW,
        name: "Rainbow (formula)".into(),
        attribute: A::Red,
        shape: Shape::Custom,
        speed: 0.25,
        table: Some(std::sync::Arc::new(table)),
        params: out.params,
        formula: FORMULA.into(),
        ..Default::default()
    }
}

