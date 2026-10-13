//! Audio effect processing for cue effect chains ([`crate::effects::AudioEffect`]).
//!
//! An [`FxChain`] is built off the audio thread (all buffers allocated up front, sized for the
//! largest setting) and handed to a mixer voice, which runs it on every frame of the cue's
//! source channels after the EQ. A live edit builds a new chain; the voice moves the old
//! chain's state (delay lines, reverb tails, envelopes) into it with [`FxChain::adopt`], which
//! only swaps buffers, so tails carry on through the edit and nothing allocates or frees on the
//! audio thread.
//!
//! Dynamics effects (compressor, limiter, gate) link their channels: one detector on the
//! loudest channel sets the gain for all, so the stereo image doesn't wander.

use crate::effects::{AudioEffect, AudioFxKind, MAX_AUDIO_FX};
use crate::model::db_to_gain;

/// One-pole smoothing coefficient for a time constant of `ms` at `rate`.
fn coef(ms: f32, rate: f32) -> f32 {
    if ms <= 0.0 { 0.0 } else { (-1.0 / (ms * 0.001 * rate)).exp() }
}

/// One-pole low-pass coefficient for a cutoff of `hz`.
fn lp_coef(hz: f32, rate: f32) -> f32 {
    1.0 - (-std::f32::consts::TAU * hz.min(rate * 0.49) / rate).exp()
}

#[inline]
fn flush(x: f32) -> f32 {
    if x.abs() < 1e-20 { 0.0 } else { x }
}

/// A circular buffer per channel, read at fractional delays.
#[derive(Default)]
struct Lines {
    buf: Vec<f32>,
    len: usize,
    pos: usize,
}

impl Lines {
    fn new(channels: usize, len: usize) -> Self {
        Self { buf: vec![0.0; channels * len.max(2)], len: len.max(2), pos: 0 }
    }

    /// Sample written `delay` frames ago on channel `c` (linear interpolation).
    #[inline]
    fn read(&self, c: usize, delay: f32) -> f32 {
        let d = delay.clamp(1.0, (self.len - 2) as f32);
        let i = d as usize;
        let frac = d - i as f32;
        let at = |k: usize| self.buf[c * self.len + (self.pos + self.len - k) % self.len];
        at(i) * (1.0 - frac) + at(i + 1) * frac
    }

    #[inline]
    fn write(&mut self, c: usize, v: f32) {
        self.buf[c * self.len + self.pos] = flush(v);
    }

    #[inline]
    fn advance(&mut self) {
        self.pos = (self.pos + 1) % self.len;
    }
}

struct Delay {
    delay: f32,
    feedback: f32,
    lp: f32,
    mix: f32,
    st: DelayState,
}

#[derive(Default)]
struct DelayState {
    lines: Lines,
    lp: Vec<f32>,
    /// Current delay in frames, gliding to the setting (no jumps when the time is edited).
    cur: f32,
}

/// Freeverb tunings at 44.1 kHz.
const COMBS: [usize; 8] = [1116, 1188, 1277, 1356, 1422, 1491, 1557, 1617];
const ALLPASSES: [usize; 4] = [556, 441, 341, 225];
/// Extra length on odd channels, so channels decorrelate.
const SPREAD: usize = 23;

#[derive(Default)]
struct Comb {
    buf: Vec<f32>,
    pos: usize,
    store: f32,
}

#[derive(Default)]
struct Allpass {
    buf: Vec<f32>,
    pos: usize,
}

struct Reverb {
    feedback: f32,
    damp: f32,
    pre: usize,
    width: f32,
    mix: f32,
    st: ReverbState,
}

#[derive(Default)]
struct ReverbState {
    /// Per channel: 8 combs then 4 all-passes.
    combs: Vec<Comb>,
    allpasses: Vec<Allpass>,
    pre: Lines,
    wet: Vec<f32>,
}

impl ReverbState {
    fn new(channels: usize, rate: f32) -> Self {
        let scale = rate / 44100.0;
        let len = |n: usize, c: usize| ((n + if c % 2 == 1 { SPREAD } else { 0 }) as f32 * scale) as usize + 1;
        let mut s = Self { pre: Lines::new(channels, (0.21 * rate) as usize), wet: vec![0.0; channels], ..Default::default() };
        for c in 0..channels {
            s.combs.extend(COMBS.iter().map(|n| Comb { buf: vec![0.0; len(*n, c)], ..Default::default() }));
            s.allpasses.extend(ALLPASSES.iter().map(|n| Allpass { buf: vec![0.0; len(*n, c)], pos: 0 }));
        }
        s
    }
}

/// Shared shape of the linked dynamics processors.
struct Dynamics {
    /// Compressor: threshold, ratio, makeup (dB). Limiter: ceiling (linear).
    a: f32,
    b: f32,
    c: f32,
    attack: f32,
    release: f32,
    st: DynState,
}

#[derive(Default)]
struct DynState {
    /// Compressor: gain reduction in dB. Limiter/gate: linear gain.
    env: f32,
    /// Gate: detector envelope and frames left to hold open.
    level: f32,
    hold: u32,
}

struct Gate {
    threshold: f32,
    floor: f32,
    attack: f32,
    release: f32,
    hold: u32,
    detect: f32,
    st: DynState,
}

struct Distortion {
    drive: f32,
    lp: f32,
    out: f32,
    mix: f32,
    st: Vec<f32>,
}

struct Chorus {
    rate: f32,
    base: f32,
    depth: f32,
    feedback: f32,
    mix: f32,
    st: ChorusState,
}

#[derive(Default)]
struct ChorusState {
    lines: Lines,
    phase: f32,
}

struct Crusher {
    step: f32,
    every: f32,
    mix: f32,
    st: CrushState,
}

#[derive(Default)]
struct CrushState {
    held: Vec<f32>,
    count: f32,
}

enum Unit {
    Delay(Delay),
    Reverb(Box<Reverb>),
    Compressor(Dynamics),
    Limiter(Dynamics),
    Gate(Gate),
    Distortion(Distortion),
    Chorus(Chorus),
    Crusher(Crusher),
}

/// A cue's audio effects, ready to run on the audio thread.
pub struct FxChain {
    /// Every effect of the list (bypassed ones too, so positions stay put for [`FxChain::adopt`]).
    units: Vec<(bool, AudioFxKind, Unit)>,
    channels: usize,
    rate: f32,
}

impl FxChain {
    /// Build the chain for `fx` running on `channels` channels at `rate` (None when no effect
    /// is enabled). Allocates: call it off the audio thread.
    pub fn new(fx: &[AudioEffect], channels: usize, rate: f32) -> Option<Box<FxChain>> {
        if !fx.iter().any(|e| e.enabled) {
            return None;
        }
        let ch = channels.max(1);
        let units = fx.iter().take(MAX_AUDIO_FX).map(|e| (e.enabled, e.kind, unit(e, ch, rate))).collect();
        Some(Box::new(FxChain { units, channels: ch, rate }))
    }

    /// Take over the running state of `old` (the chain this one replaces), unit by unit where the
    /// effect at the same place is of the same kind. Only swaps buffers (real-time safe).
    pub fn adopt(&mut self, old: &mut FxChain) {
        if old.channels != self.channels || old.rate != self.rate {
            return;
        }
        for ((_, ka, a), (_, kb, b)) in self.units.iter_mut().zip(old.units.iter_mut()) {
            if ka != kb {
                continue;
            }
            match (a, b) {
                (Unit::Delay(a), Unit::Delay(b)) => std::mem::swap(&mut a.st, &mut b.st),
                (Unit::Reverb(a), Unit::Reverb(b)) => std::mem::swap(&mut a.st, &mut b.st),
                (Unit::Compressor(a), Unit::Compressor(b)) | (Unit::Limiter(a), Unit::Limiter(b)) => {
                    std::mem::swap(&mut a.st, &mut b.st)
                }
                (Unit::Gate(a), Unit::Gate(b)) => std::mem::swap(&mut a.st, &mut b.st),
                (Unit::Distortion(a), Unit::Distortion(b)) => std::mem::swap(&mut a.st, &mut b.st),
                (Unit::Chorus(a), Unit::Chorus(b)) => std::mem::swap(&mut a.st, &mut b.st),
                (Unit::Crusher(a), Unit::Crusher(b)) => std::mem::swap(&mut a.st, &mut b.st),
                _ => {}
            }
        }
    }

    /// Run one frame (`x` holds one sample per channel) through every enabled effect in order.
    #[inline]
    pub fn process(&mut self, x: &mut [f32]) {
        let n = x.len().min(self.channels);
        let x = &mut x[..n];
        for (on, _, u) in self.units.iter_mut() {
            if *on {
                u.process(x);
            }
        }
    }
}

fn unit(e: &AudioEffect, ch: usize, rate: f32) -> Unit {
    let v = |i: usize| e.get(i);
    let pct = |i: usize| e.get(i) / 100.0;
    match e.kind {
        AudioFxKind::Delay => Unit::Delay(Delay {
            delay: v(0) * 0.001 * rate,
            feedback: pct(1),
            lp: lp_coef(v(2), rate),
            mix: pct(3),
            st: DelayState { lines: Lines::new(ch, (2.05 * rate) as usize), lp: vec![0.0; ch], cur: v(0) * 0.001 * rate },
        }),
        AudioFxKind::Reverb => Unit::Reverb(Box::new(Reverb {
            feedback: 0.7 + 0.28 * pct(0),
            damp: 0.4 * pct(1),
            pre: (v(2) * 0.001 * rate) as usize,
            width: pct(3),
            mix: pct(4),
            st: ReverbState::new(ch, rate),
        })),
        AudioFxKind::Compressor => Unit::Compressor(Dynamics {
            a: v(0),
            b: v(1).max(1.0),
            c: v(4),
            attack: coef(v(2), rate),
            release: coef(v(3), rate),
            st: DynState::default(),
        }),
        AudioFxKind::Limiter => Unit::Limiter(Dynamics {
            a: db_to_gain(v(0)),
            b: 0.0,
            c: 0.0,
            attack: 0.0,
            release: coef(v(1), rate),
            st: DynState { env: 1.0, ..Default::default() },
        }),
        AudioFxKind::Gate => Unit::Gate(Gate {
            threshold: db_to_gain(v(0)),
            floor: db_to_gain(-v(1)),
            attack: coef(v(2), rate),
            release: coef(v(4), rate),
            hold: (v(3) * 0.001 * rate) as u32,
            detect: coef(5.0, rate),
            st: DynState { env: 1.0, ..Default::default() },
        }),
        AudioFxKind::Distortion => Unit::Distortion(Distortion {
            drive: db_to_gain(v(0)),
            lp: lp_coef(v(1), rate),
            out: db_to_gain(v(2)),
            mix: pct(3),
            st: vec![0.0; ch],
        }),
        AudioFxKind::Chorus => Unit::Chorus(Chorus {
            rate: std::f32::consts::TAU * v(0) / rate,
            base: v(2) * 0.001 * rate,
            depth: v(1) * 0.001 * rate,
            feedback: pct(3),
            mix: pct(4),
            st: ChorusState { lines: Lines::new(ch, (0.045 * rate) as usize), phase: 0.0 },
        }),
        AudioFxKind::Bitcrusher => Unit::Crusher(Crusher {
            step: 2.0 / 2f32.powf(v(0).round()),
            every: v(1).max(1.0),
            mix: pct(2),
            st: CrushState { held: vec![0.0; ch], count: f32::MAX },
        }),
    }
}

impl Unit {
    #[inline]
    fn process(&mut self, x: &mut [f32]) {
        match self {
            Unit::Delay(d) => {
                let st = &mut d.st;
                st.cur += (d.delay - st.cur) * 0.0005;
                for (c, s) in x.iter_mut().enumerate() {
                    let wet = st.lines.read(c, st.cur);
                    st.lp[c] = flush(st.lp[c] + d.lp * (wet - st.lp[c]));
                    st.lines.write(c, *s + d.feedback * st.lp[c]);
                    *s = *s * (1.0 - d.mix) + wet * d.mix;
                }
                st.lines.advance();
            }
            Unit::Reverb(r) => {
                let n = x.len();
                let st = &mut r.st;
                let mut sum = 0.0;
                for (c, s) in x.iter().enumerate() {
                    st.pre.write(c, *s);
                    let input = if r.pre == 0 { *s } else { st.pre.read(c, r.pre as f32) } * 0.015;
                    let mut out = 0.0;
                    for comb in &mut st.combs[c * 8..c * 8 + 8] {
                        let y = comb.buf[comb.pos];
                        comb.store = flush(y * (1.0 - r.damp) + comb.store * r.damp);
                        comb.buf[comb.pos] = flush(input + comb.store * r.feedback);
                        comb.pos = (comb.pos + 1) % comb.buf.len();
                        out += y;
                    }
                    for ap in &mut st.allpasses[c * 4..c * 4 + 4] {
                        let b = ap.buf[ap.pos];
                        ap.buf[ap.pos] = flush(out + b * 0.5);
                        ap.pos = (ap.pos + 1) % ap.buf.len();
                        out = b - out;
                    }
                    st.wet[c] = out * 3.0;
                    sum += st.wet[c];
                }
                st.pre.advance();
                // Width: each channel's own tail against the others' (Freeverb's wet1/wet2).
                let w1 = 0.5 + r.width * 0.5;
                for (c, s) in x.iter_mut().enumerate() {
                    let wet = if n > 1 { w1 * st.wet[c] + (1.0 - w1) * (sum - st.wet[c]) / (n - 1) as f32 } else { st.wet[c] };
                    *s = *s * (1.0 - r.mix) + wet * r.mix;
                }
            }
            Unit::Compressor(d) => {
                let peak = x.iter().fold(0f32, |m, s| m.max(s.abs()));
                let level = 20.0 * peak.max(1e-9).log10();
                let over = level - d.a;
                const KNEE: f32 = 6.0;
                let slope = 1.0 / d.b - 1.0;
                let target = if 2.0 * over < -KNEE {
                    0.0
                } else if 2.0 * over <= KNEE {
                    slope * (over + KNEE / 2.0).powi(2) / (2.0 * KNEE)
                } else {
                    slope * over
                };
                let k = if target < d.st.env { d.attack } else { d.release };
                d.st.env = target + k * (d.st.env - target);
                let g = db_to_gain(d.st.env + d.c);
                x.iter_mut().for_each(|s| *s *= g);
            }
            Unit::Limiter(d) => {
                let peak = x.iter().fold(0f32, |m, s| m.max(s.abs()));
                let target = if peak > d.a { d.a / peak } else { 1.0 };
                d.st.env = if target < d.st.env { target } else { target + d.release * (d.st.env - target) };
                let (g, ceil) = (d.st.env, d.a);
                x.iter_mut().for_each(|s| *s = (*s * g).clamp(-ceil, ceil));
            }
            Unit::Gate(g) => {
                let peak = x.iter().fold(0f32, |m, s| m.max(s.abs()));
                let st = &mut g.st;
                st.level = if peak > st.level { peak } else { peak + g.detect * (st.level - peak) };
                if st.level >= g.threshold {
                    st.hold = g.hold;
                } else {
                    st.hold = st.hold.saturating_sub(1);
                }
                let open = st.level >= g.threshold || st.hold > 0;
                let target = if open { 1.0 } else { g.floor };
                let k = if target > st.env { g.attack } else { g.release };
                st.env = target + k * (st.env - target);
                x.iter_mut().for_each(|s| *s *= st.env);
            }
            Unit::Distortion(d) => {
                for (c, s) in x.iter_mut().enumerate() {
                    let y = (*s * d.drive).tanh();
                    d.st[c] = flush(d.st[c] + d.lp * (y - d.st[c]));
                    *s = *s * (1.0 - d.mix) + d.st[c] * d.out * d.mix;
                }
            }
            Unit::Chorus(ch) => {
                let st = &mut ch.st;
                let n = x.len().max(1) as f32;
                for (c, s) in x.iter_mut().enumerate() {
                    // Channels sweep out of phase, which widens the sound.
                    let lfo = (st.phase + std::f32::consts::TAU * c as f32 / n).sin();
                    let wet = st.lines.read(c, ch.base + ch.depth * (0.5 + 0.5 * lfo));
                    st.lines.write(c, *s + wet * ch.feedback);
                    *s = *s * (1.0 - ch.mix) + wet * ch.mix;
                }
                st.lines.advance();
                st.phase = (st.phase + ch.rate) % std::f32::consts::TAU;
            }
            Unit::Crusher(b) => {
                let st = &mut b.st;
                st.count += 1.0;
                let take = st.count >= b.every;
                if take {
                    st.count = 0.0;
                }
                for (c, s) in x.iter_mut().enumerate() {
                    if take {
                        st.held[c] = (*s / b.step).round() * b.step;
                    }
                    *s = *s * (1.0 - b.mix) + st.held[c] * b.mix;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f32 = 48000.0;

    fn run(fx: &[AudioEffect], ch: usize, input: impl Fn(usize, usize) -> f32, frames: usize) -> Vec<Vec<f32>> {
        let mut chain = FxChain::new(fx, ch, RATE).expect("enabled");
        (0..frames)
            .map(|f| {
                let mut x: Vec<f32> = (0..ch).map(|c| input(f, c)).collect();
                chain.process(&mut x);
                x
            })
            .collect()
    }

    fn fx(kind: AudioFxKind, set: &[(usize, f32)]) -> AudioEffect {
        let mut e = AudioEffect::new(kind);
        for (i, v) in set {
            e.params[*i] = *v;
        }
        e
    }

    #[test]
    fn nothing_enabled_builds_no_chain() {
        assert!(FxChain::new(&[], 2, RATE).is_none());
        assert!(FxChain::new(&[AudioEffect { enabled: false, ..Default::default() }], 2, RATE).is_none());
    }

    #[test]
    fn delay_echoes_after_its_time() {
        // 10 ms, fully wet, no feedback: an impulse comes back 480 frames later.
        let d = fx(AudioFxKind::Delay, &[(0, 10.0), (1, 0.0), (2, 20000.0), (3, 100.0)]);
        let out = run(&[d], 1, |f, _| if f == 0 { 1.0 } else { 0.0 }, 1000);
        let peak = (0..1000).max_by(|a, b| out[*a][0].abs().total_cmp(&out[*b][0].abs())).unwrap();
        assert!((479..=481).contains(&peak), "{peak}");
    }

    #[test]
    fn reverb_rings_on_after_the_input_stops() {
        let r = fx(AudioFxKind::Reverb, &[(2, 0.0), (4, 100.0)]);
        let out = run(&[r], 2, |f, _| if f < 100 { 0.5 } else { 0.0 }, 24000);
        let tail: f32 = out[12000..].iter().map(|x| x[0].abs()).sum();
        assert!(tail > 0.01, "{tail}");
        assert!(out.iter().all(|x| x.iter().all(|s| s.is_finite() && s.abs() < 2.0)));
    }

    #[test]
    fn compressor_and_limiter_tame_loud_signals() {
        let sine = |f: usize, _| 0.9 * (f as f32 * 0.05).sin();
        let comp = fx(AudioFxKind::Compressor, &[(0, -20.0), (1, 10.0), (2, 1.0), (3, 50.0), (4, 0.0)]);
        let out = run(&[comp], 2, sine, 24000);
        let peak = out[12000..].iter().fold(0f32, |m, x| m.max(x[0].abs()));
        assert!(peak < 0.3, "{peak}");
        let lim = fx(AudioFxKind::Limiter, &[(0, -6.0)]);
        let out = run(&[lim], 2, sine, 4800);
        assert!(out.iter().all(|x| x[0].abs() <= 0.502), "limiter let a peak through");
    }

    #[test]
    fn gate_closes_on_quiet_input() {
        let g = fx(AudioFxKind::Gate, &[(0, -30.0), (1, 60.0), (3, 0.0), (4, 10.0)]);
        let out = run(&[g], 1, |f, _| if f < 4800 { 0.5 } else { 0.001 }, 24000);
        assert!(out[100][0] > 0.4);
        assert!(out[23999][0].abs() < 0.0001, "{}", out[23999][0]);
    }

    #[test]
    fn adopting_keeps_a_delay_tail() {
        let d = fx(AudioFxKind::Delay, &[(0, 10.0), (1, 0.0), (3, 100.0)]);
        let mut a = FxChain::new(&[d], 1, RATE).unwrap();
        let mut x = [1.0];
        a.process(&mut x);
        // A mix edit mid-echo: the new chain still plays the echo written by the old one.
        let mut b = FxChain::new(&[fx(AudioFxKind::Delay, &[(0, 10.0), (1, 0.0), (3, 50.0)])], 1, RATE).unwrap();
        b.adopt(&mut a);
        let mut heard = 0f32;
        for _ in 0..600 {
            let mut x = [0.0];
            b.process(&mut x);
            heard = heard.max(x[0].abs());
        }
        assert!(heard > 0.4, "{heard}");
    }

    #[test]
    fn every_effect_stays_finite_and_bounded() {
        for k in AudioFxKind::ALL {
            let out = run(&[AudioEffect::new(k)], 3, |f, c| ((f * (c + 1)) as f32 * 0.031).sin() * 0.8, 9600);
            for x in &out {
                for s in x {
                    assert!(s.is_finite() && s.abs() < 4.0, "{k:?}: {s}");
                }
            }
        }
    }
}
