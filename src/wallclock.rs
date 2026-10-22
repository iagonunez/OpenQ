//! Wall-clock triggers: cues that fire at a time of day on this computer's clock and time zone.
//!
//! [`Watch`] reads the local time a few times a second and fires the armed cues whose time the
//! clock has just passed, steadily: a jump (the clock set, the computer waking up) fires nothing,
//! like timecode jumps. Midnight is crossed with the right weekday on each side.

use crate::model::{CueId, CueList, WallClockTrigger};
use std::time::{Duration, Instant};

/// How often the local time is read.
const CHECK: Duration = Duration::from_millis(200);
/// A step of the clock longer than this is a jump, not running time.
const MAX_STEP: u32 = 3;

/// Local time of day in whole seconds and the weekday (0 = Monday).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DayTime {
    /// Seconds after midnight.
    pub secs: u32,
    /// 0 = Monday ... 6 = Sunday.
    pub weekday: u8,
}

/// The time now on this computer's clock, in its time zone.
pub fn now() -> DayTime {
    let z = jiff::Zoned::now();
    let secs = z.hour() as u32 * 3600 + z.minute() as u32 * 60 + z.second() as u32;
    DayTime { secs, weekday: z.weekday().to_monday_zero_offset() as u8 }
}

/// Short weekday name (0 = Monday).
pub fn day_name(d: u8) -> &'static str {
    match d {
        0 => crate::i18n::t("Mon"),
        1 => crate::i18n::t("Tue"),
        2 => crate::i18n::t("Wed"),
        3 => crate::i18n::t("Thu"),
        4 => crate::i18n::t("Fri"),
        5 => crate::i18n::t("Sat"),
        _ => crate::i18n::t("Sun"),
    }
}

/// Triggers due as the clock ran from `from` (already handled) to `to`: times in (from, to],
/// across midnight too. Nothing when the clock didn't run steadily forward.
pub fn due(triggers: &[(CueId, WallClockTrigger)], from: DayTime, to: DayTime) -> Vec<CueId> {
    const DAY: u32 = 86_400;
    let step = if to.secs >= from.secs && to.weekday == from.weekday {
        to.secs - from.secs
    } else if to.weekday == (from.weekday + 1) % 7 {
        to.secs + DAY - from.secs
    } else {
        u32::MAX
    };
    if step == 0 || step > MAX_STEP {
        return Vec::new();
    }
    let hit = |t: &WallClockTrigger| {
        if to.secs >= from.secs && to.weekday == from.weekday {
            t.time > from.secs && t.time <= to.secs && t.on_day(to.weekday)
        } else {
            (t.time > from.secs && t.on_day(from.weekday)) || (t.time <= to.secs && t.on_day(to.weekday))
        }
    };
    triggers.iter().filter(|(_, t)| hit(t)).map(|(id, _)| *id).collect()
}

/// Every cue with a wall-clock trigger, in any list.
pub fn triggers(list: &CueList) -> Vec<(CueId, WallClockTrigger)> {
    list.iter_all().into_iter().filter_map(|c| Some((c.id, c.wallclock_trigger?))).collect()
}

/// Watches the clock for a show.
#[derive(Default)]
pub struct Watch {
    /// The cues with triggers, for an edit generation.
    cues: Option<(u64, Vec<(CueId, WallClockTrigger)>)>,
    last: Option<DayTime>,
    next_check: Option<Instant>,
}

impl Watch {
    /// Cues whose time has just come. `edit_gen` says when to look at the cue list again.
    pub fn poll(&mut self, list: &CueList, edit_gen: u64, now: Instant) -> Vec<CueId> {
        if self.next_check.is_some_and(|t| now < t) {
            return Vec::new();
        }
        self.next_check = Some(now + CHECK);
        if self.cues.as_ref().is_none_or(|(g, _)| *g != edit_gen) {
            self.cues = Some((edit_gen, triggers(list)));
        }
        let cues = &self.cues.as_ref().expect("just set").1;
        if cues.is_empty() {
            self.last = None;
            return Vec::new();
        }
        let t = self::now();
        let out = match self.last {
            Some(from) if from != t => due(cues, from, t),
            _ => Vec::new(),
        };
        self.last = Some(t);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: u32, weekday: u8) -> DayTime {
        DayTime { secs, weekday }
    }

    #[test]
    fn fires_once_as_the_clock_passes_and_never_on_jumps() {
        let id = CueId::new_v4();
        let trig = [(id, WallClockTrigger { time: 3600, days: 0 })];
        assert_eq!(due(&trig, at(3599, 2), at(3600, 2)), vec![id]);
        assert!(due(&trig, at(3600, 2), at(3601, 2)).is_empty(), "only once");
        assert!(due(&trig, at(3000, 2), at(3700, 2)).is_empty(), "a jump fires nothing");
        assert!(due(&trig, at(3601, 2), at(3599, 2)).is_empty(), "backwards");
    }

    #[test]
    fn weekdays_and_midnight() {
        let (a, b) = (CueId::new_v4(), CueId::new_v4());
        // a: 23:59:59 on Mondays; b: midnight on Tuesdays.
        let trig = [(a, WallClockTrigger { time: 86_399, days: 1 }), (b, WallClockTrigger { time: 0, days: 2 })];
        assert_eq!(due(&trig, at(86_398, 0), at(1, 1)), vec![a, b]);
        assert!(due(&trig, at(86_398, 1), at(1, 2)).is_empty(), "Tuesday night, Wednesday morning");
        assert_eq!(WallClockTrigger { time: 3723, days: 0b0010001 }.label(), "01:02:03 · Mon Fri");
    }
}
