//! SMPTE timecode: labels, LTC (the audio signal) and MIDI Time Code.
//!
//! Timecode values in the model are *label seconds*: `hh*3600 + mm*60 + ss + ff/fps_nominal`,
//! i.e. what the timecode reads, not how much real time has passed. For 29.97 drop-frame the two
//! differ slightly; [`Timecode::from_frame_count`] and [`Timecode::frame_count`] convert between
//! labels and the running frame count with drop-frame numbering.
//!
//! LTC frames are 80 bits sent with biphase-mark coding: every bit cell starts with a level
//! change, and a `1` has a second one in the middle. The last 16 bits are a fixed sync word,
//! which tells a reader where a frame ends.

use crate::model::TimecodeRate;

/// One timecode label.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Timecode {
    /// Hours (0–23).
    pub h: u8,
    /// Minutes.
    pub m: u8,
    /// Seconds.
    pub s: u8,
    /// Frames.
    pub f: u8,
}

impl Timecode {
    /// Label for label-seconds `secs` (wraps at 24 h).
    pub fn from_secs(secs: f64, rate: TimecodeRate) -> Self {
        let nominal = rate.nominal() as f64;
        let total = (secs.max(0.0) * nominal + 1e-6).floor() as u64;
        let f = (total % rate.nominal() as u64) as u8;
        let s = total / rate.nominal() as u64;
        Self { h: ((s / 3600) % 24) as u8, m: ((s / 60) % 60) as u8, s: (s % 60) as u8, f }
    }

    /// Label-seconds of this label.
    pub fn secs(self, rate: TimecodeRate) -> f64 {
        self.h as f64 * 3600.0 + self.m as f64 * 60.0 + self.s as f64 + self.f as f64 / rate.nominal() as f64
    }

    /// Frames since 00:00:00:00, counting only frame numbers that exist (drop-frame skips some).
    pub fn frame_count(self, rate: TimecodeRate) -> u64 {
        let n = rate.nominal() as u64;
        let mins = self.h as u64 * 60 + self.m as u64;
        let raw = (mins * 60 + self.s as u64) * n + self.f as u64;
        if rate.drop_frame() { raw - 2 * (mins - mins / 10) } else { raw }
    }

    /// The label `count` frames after 00:00:00:00.
    pub fn from_frame_count(count: u64, rate: TimecodeRate) -> Self {
        let n = rate.nominal() as u64;
        let mut c = count % (24 * 3600 * n - if rate.drop_frame() { 2 * (24 * 60 - 24 * 6) } else { 0 });
        if rate.drop_frame() {
            // Frames 0 and 1 are skipped at the start of every minute except each tenth.
            let per10 = 10 * 60 * n - 18;
            let d = c / per10;
            let r = c % per10;
            c += 18 * d + if r > 1 { 2 * ((r - 2) / (60 * n - 2)) } else { 0 };
        }
        let f = (c % n) as u8;
        let s = c / n;
        Self { h: ((s / 3600) % 24) as u8, m: ((s / 60) % 60) as u8, s: (s % 60) as u8, f }
    }

    /// The next label.
    pub fn next(self, rate: TimecodeRate) -> Self {
        Self::from_frame_count(self.frame_count(rate) + 1, rate)
    }

    /// "hh:mm:ss:ff" (";" before the frames for drop-frame).
    pub fn format(self, rate: TimecodeRate) -> String {
        let sep = if rate.drop_frame() { ';' } else { ':' };
        format!("{:02}:{:02}:{:02}{sep}{:02}", self.h, self.m, self.s, self.f)
    }

    /// Parse "hh:mm:ss:ff" (any of `:;.` as separators; missing leading fields are zero).
    pub fn parse(text: &str, rate: TimecodeRate) -> Option<Self> {
        let parts: Vec<u32> = text.trim().split([':', ';', '.']).map(|p| p.trim().parse().ok()).collect::<Option<_>>()?;
        if parts.is_empty() || parts.len() > 4 {
            return None;
        }
        let mut v = [0u32; 4];
        v[4 - parts.len()..].copy_from_slice(&parts);
        let [h, m, s, f] = v;
        (h < 24 && m < 60 && s < 60 && f < rate.nominal()).then_some(Self { h: h as u8, m: m as u8, s: s as u8, f: f as u8 })
    }
}

/// Format label-seconds as timecode.
pub fn format_secs(secs: f64, rate: TimecodeRate) -> String {
    Timecode::from_secs(secs, rate).format(rate)
}

/// Parse timecode text to label-seconds.
pub fn parse_secs(text: &str, rate: TimecodeRate) -> Option<f64> {
    Timecode::parse(text, rate).map(|t| t.secs(rate))
}

// ------------------------------------------------------------------ LTC

/// LTC sync word, in transmission order (bits 64..79).
const SYNC: [bool; 16] =
    [false, false, true, true, true, true, true, true, true, true, true, true, true, true, false, true];

/// The 80 bits of one LTC frame, in transmission order.
pub fn ltc_bits(tc: Timecode, rate: TimecodeRate) -> [bool; 80] {
    let mut b = [false; 80];
    let mut put = |at: usize, len: usize, v: u8| {
        for i in 0..len {
            b[at + i] = (v >> i) & 1 == 1;
        }
    };
    put(0, 4, tc.f % 10);
    put(8, 2, tc.f / 10);
    put(16, 4, tc.s % 10);
    put(24, 3, tc.s / 10);
    put(32, 4, tc.m % 10);
    put(40, 3, tc.m / 10);
    put(48, 4, tc.h % 10);
    put(56, 2, tc.h / 10);
    b[10] = rate.drop_frame();
    b[64..80].copy_from_slice(&SYNC);
    // Polarity correction: an even number of zeros keeps every frame starting on the same edge.
    let pc = if rate == TimecodeRate::Fps25 { 59 } else { 27 };
    if b.iter().filter(|x| !**x).count() % 2 == 1 {
        b[pc] = !b[pc];
    }
    b
}

/// Decode a frame's 80 bits (sync word last). None if the sync word is wrong.
pub fn ltc_decode(b: &[bool; 80]) -> Option<(Timecode, bool)> {
    if b[64..80] != SYNC {
        return None;
    }
    let get = |at: usize, len: usize| (0..len).fold(0u8, |v, i| v | ((b[at + i] as u8) << i));
    let tc = Timecode {
        f: get(0, 4) + 10 * get(8, 2),
        s: get(16, 4) + 10 * get(24, 3),
        m: get(32, 4) + 10 * get(40, 3),
        h: get(48, 4) + 10 * get(56, 2),
    };
    (tc.f < 30 && tc.s < 60 && tc.m < 60 && tc.h < 24).then_some((tc, b[10]))
}

/// Generates the LTC audio signal, sample by sample.
pub struct LtcEncoder {
    rate: TimecodeRate,
    sample_rate: f64,
    frame: u64,
    /// Position inside the current frame, in samples.
    pos: f64,
    bits: [bool; 80],
    level: bool,
    /// Bit cell the last sample was in (to place the transitions).
    last_half: i64,
    smooth: f32,
    amplitude: f32,
}

impl LtcEncoder {
    /// Start at `start` (label-seconds), producing `sample_rate` samples per second at linear
    /// `amplitude`.
    pub fn new(start: f64, rate: TimecodeRate, sample_rate: u32, amplitude: f32) -> Self {
        let frame = Timecode::from_secs(start, rate).frame_count(rate);
        Self {
            rate,
            sample_rate: sample_rate as f64,
            frame,
            pos: 0.0,
            bits: ltc_bits(Timecode::from_frame_count(frame, rate), rate),
            level: false,
            last_half: -1,
            smooth: 0.0,
            amplitude,
        }
    }

    /// The label being sent now.
    pub fn current(&self) -> Timecode {
        Timecode::from_frame_count(self.frame, self.rate)
    }

    /// Fill `out` with the next samples.
    pub fn fill(&mut self, out: &mut [f32]) {
        let per_frame = self.sample_rate / self.rate.fps();
        let per_half = per_frame / 160.0;
        // A short rise time (about 25 µs) instead of a hard square edge.
        let k = 1.0 - (-1.0 / (25e-6 * self.sample_rate as f32)).exp();
        for o in out {
            let half = (self.pos / per_half) as i64;
            while self.last_half < half {
                self.last_half += 1;
                let bit = (self.last_half / 2) as usize;
                // Every cell starts with a transition; a 1 also flips in the middle.
                if self.last_half % 2 == 0 || self.bits[bit.min(79)] {
                    self.level = !self.level;
                }
            }
            let target = if self.level { self.amplitude } else { -self.amplitude };
            self.smooth += (target - self.smooth) * k;
            *o = self.smooth;
            self.pos += 1.0;
            if self.pos >= per_frame {
                self.pos -= per_frame;
                self.last_half = -1;
                self.frame += 1;
                self.bits = ltc_bits(Timecode::from_frame_count(self.frame, self.rate), self.rate);
            }
        }
    }
}

/// Reads LTC from audio samples.
pub struct LtcDecoder {
    sample_rate: f64,
    /// Samples since the last zero crossing.
    since: f64,
    /// Signal is above the upper threshold (until it falls below the lower one).
    high: bool,
    /// Estimated length of a bit cell, in samples.
    cell: f64,
    /// A first half-cell is waiting for its partner.
    half_pending: bool,
    bits: [bool; 80],
}

impl LtcDecoder {
    /// A decoder for audio at `sample_rate`.
    pub fn new(sample_rate: u32) -> Self {
        let sample_rate = sample_rate as f64;
        // Start from 25 fps; the estimate adapts within a few bits.
        Self { sample_rate, since: 0.0, high: false, cell: sample_rate / (25.0 * 80.0), half_pending: false, bits: [false; 80] }
    }

    fn bit(&mut self, v: bool, out: &mut Vec<(Timecode, bool, usize)>, at: usize) {
        self.bits.copy_within(1.., 0);
        self.bits[79] = v;
        if let Some((tc, df)) = ltc_decode(&self.bits) {
            out.push((tc, df, at));
        }
    }

    /// Feed mono samples. Returns each frame completed in them: (label, drop-frame flag, index of
    /// the sample where its sync word ended).
    pub fn push(&mut self, samples: &[f32]) -> Vec<(Timecode, bool, usize)> {
        let mut out = Vec::new();
        for (i, &x) in samples.iter().enumerate() {
            self.since += 1.0;
            // Zero crossings with a little hysteresis against noise.
            const TH: f32 = 0.01;
            if (self.high && x < -TH) || (!self.high && x > TH) {
                self.high = !self.high;
            } else {
                continue;
            }
            let d = self.since;
            self.since = 0.0;
            if d > self.cell * 0.75 && d < self.cell * 1.5 {
                // A whole cell: a 0.
                self.cell = self.cell * 0.9 + d * 0.1;
                self.half_pending = false;
                self.bit(false, &mut out, i);
            } else if d > self.cell * 0.25 && d <= self.cell * 0.75 {
                self.cell = self.cell * 0.9 + 2.0 * d * 0.1;
                if self.half_pending {
                    self.half_pending = false;
                    self.bit(true, &mut out, i);
                } else {
                    self.half_pending = true;
                }
            } else {
                // Out of range: resynchronise on whatever comes next, re-estimating the cell from
                // this interval (rates from 24 to 30 fps at any sample rate are within ±50%).
                if d > 4.0 && d < self.sample_rate / (20.0 * 80.0) {
                    self.cell = d;
                }
                self.half_pending = false;
            }
        }
        out
    }
}

// ------------------------------------------------------------------ MTC

/// MTC rate code (bits 5–6 of the hours byte).
fn mtc_rate_code(rate: TimecodeRate) -> u8 {
    match rate {
        TimecodeRate::Fps24 => 0,
        TimecodeRate::Fps25 => 1,
        TimecodeRate::Fps2997Df => 2,
        TimecodeRate::Fps30 => 3,
    }
}

fn mtc_rate(code: u8) -> TimecodeRate {
    match code & 3 {
        0 => TimecodeRate::Fps24,
        1 => TimecodeRate::Fps25,
        2 => TimecodeRate::Fps2997Df,
        _ => TimecodeRate::Fps30,
    }
}

/// The 2-byte quarter-frame message `piece` (0–7) for `tc`.
pub fn mtc_quarter_frame(tc: Timecode, rate: TimecodeRate, piece: u8) -> [u8; 2] {
    let nib = match piece {
        0 => tc.f & 0xF,
        1 => tc.f >> 4,
        2 => tc.s & 0xF,
        3 => tc.s >> 4,
        4 => tc.m & 0xF,
        5 => tc.m >> 4,
        6 => tc.h & 0xF,
        _ => (tc.h >> 4) | (mtc_rate_code(rate) << 1),
    };
    [0xF1, (piece << 4) | nib]
}

/// A full-frame (locate) SysEx message for `tc`.
pub fn mtc_full_frame(tc: Timecode, rate: TimecodeRate) -> [u8; 10] {
    [0xF0, 0x7F, 0x7F, 0x01, 0x01, tc.h | (mtc_rate_code(rate) << 5), tc.m, tc.s, tc.f, 0xF7]
}

/// Reassembles MTC from quarter-frame and full-frame messages.
#[derive(Default)]
pub struct MtcDecoder {
    pieces: [u8; 8],
    seen: u8,
}

impl MtcDecoder {
    /// Feed one MIDI message. Returns a label (and rate) when one is complete. Quarter-frame
    /// labels are reported two frames on, the time when their last piece arrives.
    pub fn push(&mut self, msg: &[u8]) -> Option<(Timecode, TimecodeRate)> {
        match msg {
            [0xF1, d] => {
                let piece = (d >> 4) & 7;
                self.pieces[piece as usize] = d & 0xF;
                self.seen |= 1 << piece;
                if piece == 7 && self.seen == 0xFF {
                    self.seen = 0;
                    let p = &self.pieces;
                    let rate = mtc_rate(p[7] >> 1);
                    let tc = Timecode { f: p[0] | (p[1] << 4), s: p[2] | (p[3] << 4), m: p[4] | (p[5] << 4), h: p[6] | ((p[7] & 1) << 4) };
                    // The pieces describe the frame when piece 0 went out, two frames ago.
                    return Some((tc.next(rate).next(rate), rate));
                }
                None
            }
            [0xF0, 0x7F, _, 0x01, 0x01, h, m, s, f, 0xF7] => {
                self.seen = 0;
                let rate = mtc_rate(h >> 5);
                Some((Timecode { h: h & 0x1F, m: *m, s: *s, f: *f }, rate))
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_format_parse_and_count() {
        let r = TimecodeRate::Fps25;
        let t = Timecode::parse("01:02:03:04", r).unwrap();
        assert_eq!(t.format(r), "01:02:03:04");
        assert_eq!(format_secs(t.secs(r), r), "01:02:03:04");
        assert_eq!(Timecode::parse("3:04", r), Some(Timecode { h: 0, m: 0, s: 3, f: 4 }));
        assert_eq!(Timecode::parse("00:00:00:25", r), None);
        for n in [0, 1, 24, 25, 90_000, 2_159_999] {
            assert_eq!(Timecode::from_frame_count(n, r).frame_count(r), n);
        }
    }

    #[test]
    fn drop_frame_skips_the_right_numbers() {
        let r = TimecodeRate::Fps2997Df;
        let last = Timecode { h: 0, m: 0, s: 59, f: 29 };
        assert_eq!(last.next(r), Timecode { h: 0, m: 1, s: 0, f: 2 });
        let ninth = Timecode { h: 0, m: 9, s: 59, f: 29 };
        assert_eq!(ninth.next(r), Timecode { h: 0, m: 10, s: 0, f: 0 });
        // An hour of drop-frame is 107 892 frames.
        assert_eq!(Timecode { h: 1, m: 0, s: 0, f: 0 }.frame_count(r), 107_892);
        for n in [0, 1799, 1800, 17_982, 17_983, 107_891] {
            assert_eq!(Timecode::from_frame_count(n, r).frame_count(r), n, "{n}");
        }
        assert_eq!(last.format(r), "00:00:59;29");
    }

    #[test]
    fn ltc_bits_roundtrip_with_even_parity() {
        for rate in TimecodeRate::ALL {
            let tc = Timecode { h: 23, m: 59, s: 58, f: (rate.nominal() - 1) as u8 };
            let b = ltc_bits(tc, rate);
            assert_eq!(b.iter().filter(|x| !**x).count() % 2, 0);
            assert_eq!(ltc_decode(&b), Some((tc, rate.drop_frame())));
        }
    }

    #[test]
    fn ltc_audio_roundtrip() {
        for (rate, sr) in [(TimecodeRate::Fps25, 48_000), (TimecodeRate::Fps30, 44_100), (TimecodeRate::Fps2997Df, 48_000)] {
            let start = Timecode { h: 1, m: 0, s: 0, f: 0 }.secs(rate);
            let mut enc = LtcEncoder::new(start, rate, sr, 0.5);
            let mut dec = LtcDecoder::new(sr);
            let mut got = Vec::new();
            let mut buf = vec![0.0; 1000];
            for _ in 0..(sr as usize * 2 / 1000) {
                enc.fill(&mut buf);
                got.extend(dec.push(&buf).into_iter().map(|g| g.0));
            }
            // After locking on, every frame arrives in order.
            assert!(got.len() >= (rate.fps() * 1.5) as usize, "{rate:?}: {}", got.len());
            for w in got.windows(2) {
                assert_eq!(w[0].next(rate), w[1], "{rate:?}");
            }
            assert!(got[0].secs(rate) - start < 0.5);
        }
    }

    #[test]
    fn mtc_roundtrip() {
        let rate = TimecodeRate::Fps25;
        let tc = Timecode { h: 10, m: 20, s: 30, f: 12 };
        let mut d = MtcDecoder::default();
        assert_eq!(d.push(&mtc_full_frame(tc, rate)), Some((tc, rate)));
        let mut out = None;
        for piece in 0..8 {
            out = d.push(&mtc_quarter_frame(tc, rate, piece));
        }
        assert_eq!(out, Some((tc.next(rate).next(rate), rate)));
    }
}
