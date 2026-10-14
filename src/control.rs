//! Show-control commands shared by every way of driving the show: the UI, OSC and remote
//! devices. A replica sends these to the main device instead of applying them locally.

use crate::engine::Backend;
use crate::model::CueId;
use crate::show::Show;
use serde::{Deserialize, Serialize};
use std::time::Instant;

/// What to do to a single cue.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum CueAction {
    /// Fire the cue (pre-wait, action, continue), as if GO were pressed on it.
    Start,
    /// Stop the cue and anything it started.
    Stop,
    /// Pause the cue, freezing its time, media and fades.
    Pause,
    /// Resume a paused cue.
    Resume,
    /// Release a vamping slice so playback carries on past it.
    Devamp,
}

/// A lighting playback action (executors and masters). Design edits are not commands.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
pub enum LightControl {
    /// GO a sequence's next cue, or a given cue.
    Go(crate::light::model::SequenceId, Option<crate::light::model::LightCueId>),
    /// Fade a sequence (None = all) out over `fade` seconds.
    Release(Option<crate::light::model::SequenceId>, f32),
    /// A sequence's master level (0..=1).
    Level(crate::light::model::SequenceId, f32),
    /// Grand master (0..=1).
    GrandMaster(f32),
    /// Blackout on/off.
    Blackout(bool),
    /// Effects tempo, BPM.
    Tempo(f32),
}

/// A transport or playhead action, independent of where it came from.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
pub enum Command {
    /// Fire the cue at the playhead and advance the playhead.
    Go,
    /// Stop all with the workspace's panic fade (a second Stop while fading stops at once).
    Stop,
    /// Stop all instantly.
    Panic,
    /// Pause everything that is running.
    PauseAll,
    /// Resume everything that is paused.
    ResumeAll,
    /// Move the playhead to the next cue in the list.
    PlayheadNext,
    /// Move the playhead to the previous cue in the list.
    PlayheadPrevious,
    /// Put the playhead on a cue (or clear it).
    SetPlayhead(Option<CueId>),
    /// Act on one cue.
    Cue(CueId, CueAction),
    /// Bring cue list `n` (tab order, from 0) forward: GO and the playhead then work on it, while
    /// cues already running in other lists carry on.
    SwitchList(usize),
    /// Lighting playback.
    Light(LightControl),
}

/// Apply a command to the show.
pub fn apply(show: &mut Show, cmd: Command, now: Instant) -> Result<(), String> {
    let Show { ws, engine, media, .. } = show;
    match cmd {
        Command::Go => engine.go(ws, media, now),
        Command::Stop => {
            if ws.lighting.stop_all_releases {
                media.light.release_all(ws.settings.panic_fade, now);
            }
            engine.stop_all(media, now, ws.settings.panic_fade)
        }
        Command::Panic => {
            if ws.lighting.stop_all_releases {
                media.light.release_all(0.0, now);
            }
            engine.stop_all(media, now, 0.0)
        }
        Command::PauseAll => engine.pause_all(media, now),
        Command::ResumeAll => engine.resume_all(media, now),
        Command::PlayheadNext | Command::PlayheadPrevious => {
            let rows = ws.cue_list.visible_rows(&|_| false);
            let i = engine.playhead.and_then(|p| rows.iter().position(|r| r.1 == p));
            let next = match (cmd, i) {
                (Command::PlayheadNext, Some(i)) => rows.get(i + 1),
                (Command::PlayheadPrevious, Some(i)) if i > 0 => rows.get(i - 1),
                (_, Some(_)) => None,
                _ => rows.first(),
            };
            if let Some(r) = next {
                engine.set_playhead(Some(r.1));
            }
        }
        Command::SetPlayhead(id) => {
            if id.is_some_and(|id| ws.cue_list.find(id).is_none()) {
                return Err("no such cue".into());
            }
            engine.set_playhead(id);
        }
        Command::SwitchList(tab) => {
            if tab >= ws.cue_list.list_count() {
                return Err("no such cue list".into());
            }
            let ph = ws.cue_list.switch_to(tab, engine.playhead);
            engine.set_playhead(ph.or(ws.cue_list.first()));
            show.edited();
        }
        Command::Light(c) => {
            let l = &mut media.light;
            match c {
                LightControl::Go(seq, cue) => l.go(seq, cue, now),
                LightControl::Release(Some(seq), fade) => l.release(seq, fade as f64, now),
                LightControl::Release(None, fade) => l.release_all(fade as f64, now),
                LightControl::Level(seq, v) => l.set_level(seq, v),
                LightControl::GrandMaster(v) => l.set_grand_master(v),
                LightControl::Blackout(b) => l.set_blackout(b),
                LightControl::Tempo(bpm) => l.set_tempo(bpm),
            }
        }
        Command::Cue(id, action) => {
            if ws.cue_list.find(id).is_none() {
                return Err("no such cue".into());
            }
            match action {
                CueAction::Start => engine.start_cue(ws, media, id, now),
                CueAction::Stop => engine.stop_cue(ws, media, id),
                CueAction::Pause => engine.pause_cue(ws, media, id, now),
                CueAction::Resume => engine.resume_cue(ws, media, id, now),
                CueAction::Devamp => {
                    if !media.devamp(id) {
                        return Err("cue is not playing".into());
                    }
                }
            }
        }
    }
    Ok(())
}
