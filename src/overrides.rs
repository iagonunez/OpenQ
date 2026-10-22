//! Overrides: global switches that cut a kind of input or output for the whole show at once (a
//! technical rehearsal with the lighting desk in charge, MIDI gear not ready, a wall-clock trigger
//! that must not fire today...). They belong to this computer's running show, not to the file: OpenQ
//! starts with everything on, and the status bar says when something is off.
//!
//! Kept in atomics so the threads that need them (OSC, MIDI input, the light thread, timecode
//! generators) read them without the show lock.

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU16, Ordering};

/// One switch.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum Override {
    /// MIDI input: cue MIDI triggers, MIDI Show Control in, control surface MIDI.
    MidiIn,
    /// MIDI output: MIDI cues, control surface feedback.
    MidiOut,
    /// OSC input (remote control on the OSC port, OSC control surfaces).
    OscIn,
    /// Network cues (OSC sent to other devices).
    NetworkOut,
    /// Timecode triggers from the cue lists' incoming timecode.
    TimecodeIn,
    /// Timecode cues: LTC is silent and MTC not sent while off (the cues still run).
    TimecodeOut,
    /// Wall-clock triggers.
    WallClock,
    /// DMX output: no packets are sent while off (the lighting keeps playing inside OpenQ).
    LightOut,
}

impl Override {
    /// Every switch, in the order the window shows them.
    pub const ALL: [Override; 8] = [
        Override::MidiIn,
        Override::MidiOut,
        Override::OscIn,
        Override::NetworkOut,
        Override::TimecodeIn,
        Override::TimecodeOut,
        Override::WallClock,
        Override::LightOut,
    ];

    fn bit(self) -> u16 {
        1 << self as u16
    }

    /// Name shown in the UI.
    pub fn label(self) -> &'static str {
        match self {
            Override::MidiIn => crate::i18n::t("MIDI input"),
            Override::MidiOut => crate::i18n::t("MIDI output"),
            Override::OscIn => crate::i18n::t("OSC input"),
            Override::NetworkOut => crate::i18n::t("Network cues"),
            Override::TimecodeIn => crate::i18n::t("Timecode triggers"),
            Override::TimecodeOut => crate::i18n::t("Timecode output"),
            Override::WallClock => crate::i18n::t("Wall-clock triggers"),
            Override::LightOut => crate::i18n::t("DMX output"),
        }
    }

    /// What turning it off does.
    pub fn help(self) -> &'static str {
        match self {
            Override::MidiIn => crate::i18n::t("MIDI triggers, MIDI Show Control and MIDI control surfaces are ignored."),
            Override::MidiOut => crate::i18n::t("MIDI cues send nothing; control surfaces get no feedback."),
            Override::OscIn => crate::i18n::t("OSC messages and OSC control surfaces are ignored."),
            Override::NetworkOut => crate::i18n::t("Network cues send nothing."),
            Override::TimecodeIn => crate::i18n::t("Incoming timecode fires no cues."),
            Override::TimecodeOut => crate::i18n::t("Timecode cues run but LTC is silent and no MTC is sent."),
            Override::WallClock => crate::i18n::t("Cues with a time of day don't fire."),
            Override::LightOut => crate::i18n::t("No DMX is sent (the lighting keeps running inside OpenQ)."),
        }
    }
}

/// Switches that are off, one bit per [`Override`] (0 = everything on).
static OFF: AtomicU16 = AtomicU16::new(0);

/// Whether `o` is on (nothing cut).
pub fn allowed(o: Override) -> bool {
    OFF.load(Ordering::Relaxed) & o.bit() == 0
}

/// Turn `o` on or off.
pub fn set(o: Override, on: bool) {
    if on {
        OFF.fetch_and(!o.bit(), Ordering::Relaxed);
    } else {
        OFF.fetch_or(o.bit(), Ordering::Relaxed);
    }
}

/// The switches that are off, as bits (sent to replicas, which show the main device's).
pub fn off_bits() -> u16 {
    OFF.load(Ordering::Relaxed)
}

/// Whether `o` is off in a set of bits from [`off_bits`].
pub fn is_off(bits: u16, o: Override) -> bool {
    bits & o.bit() != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bits_round_trip() {
        let mut bits = 0u16;
        for o in Override::ALL {
            assert!(!is_off(bits, o));
            bits |= o.bit();
            assert!(is_off(bits, o));
        }
        assert_eq!(Override::ALL.len(), Override::ALL.iter().map(|o| o.bit()).fold(0, |a, b| a | b).count_ones() as usize);
    }
}
