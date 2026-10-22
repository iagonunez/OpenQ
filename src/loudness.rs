//! Loudness (ITU-R BS.1770 / EBU R 128): the integrated loudness of a file in LUFS, measured
//! while its waveform is built, so cues can be levelled to a common loudness.
//!
//! K-weighting (a high shelf and a high-pass, designed for the file's sample rate), mean square
//! per channel over 400 ms blocks every 100 ms, channel weights (surrounds ×1.41, the LFE of a 5.1
//! file left out), then the absolute gate (−70 LUFS) and the relative gate (10 LU under the
//! loudness of the blocks above the absolute one).

/// The loudness Audio cues are levelled to by default, LUFS (EBU R 128's target).
pub const DEFAULT_TARGET: f64 = -23.0;

/// One biquad section (transposed direct form II).
#[derive(Clone, Copy, Default)]
struct Biquad {
    b: [f64; 3],
    a: [f64; 2],
    z: [f64; 2],
}

impl Biquad {
    #[inline]
    fn run(&mut self, x: f64) -> f64 {
        let y = self.b[0] * x + self.z[0];
        self.z[0] = self.b[1] * x - self.a[0] * y + self.z[1];
        self.z[1] = self.b[2] * x - self.a[1] * y;
        y
    }
}

/// The two K-weighting stages for sample rate `rate` (BS.1770's filters, redesigned for any rate).
fn k_weighting(rate: f64) -> [Biquad; 2] {
    let (f0, g, q) = (1681.974450955533, 3.999843853973347, 0.7071752369554196);
    let k = (std::f64::consts::PI * f0 / rate).tan();
    let vh = 10f64.powf(g / 20.0);
    let vb = vh.powf(0.4996667741545416);
    let a0 = 1.0 + k / q + k * k;
    let shelf = Biquad {
        b: [(vh + vb * k / q + k * k) / a0, 2.0 * (k * k - vh) / a0, (vh - vb * k / q + k * k) / a0],
        a: [2.0 * (k * k - 1.0) / a0, (1.0 - k / q + k * k) / a0],
        z: [0.0; 2],
    };
    let (f0, q) = (38.13547087602444, 0.5003270373238773);
    let k = (std::f64::consts::PI * f0 / rate).tan();
    let a0 = 1.0 + k / q + k * k;
    let hp = Biquad { b: [1.0, -2.0, 1.0], a: [2.0 * (k * k - 1.0) / a0, (1.0 - k / q + k * k) / a0], z: [0.0; 2] };
    [shelf, hp]
}

/// Channel weight: 1 for front channels, 1.41 for the surrounds of a 5.1 file, 0 for its LFE.
fn weight(channel: usize, channels: usize) -> f64 {
    match (channels, channel) {
        (6, 3) => 0.0,
        (6, 4) | (6, 5) => 1.41,
        _ => 1.0,
    }
}

/// Measures integrated loudness from samples fed in order.
pub struct Meter {
    filters: Vec<[Biquad; 2]>,
    weights: Vec<f64>,
    /// Samples per 100 ms step.
    step: usize,
    /// Weighted sum of squares of the step being filled, and its sample count.
    acc: f64,
    in_step: usize,
    /// Weighted sum of squares of every finished 100 ms step.
    steps: Vec<f64>,
}

impl Meter {
    /// A meter for `channels` channels at `rate` Hz.
    pub fn new(rate: u32, channels: usize) -> Self {
        let ch = channels.max(1);
        Self {
            filters: vec![k_weighting(rate.max(1) as f64); ch],
            weights: (0..ch).map(|c| weight(c, ch)).collect(),
            step: (rate as usize / 10).max(1),
            acc: 0.0,
            in_step: 0,
            steps: Vec::new(),
        }
    }

    /// Feed `frames` frames of planar samples (one slice per channel; extra channels are ignored).
    pub fn push(&mut self, planar: &[Vec<f32>], frames: usize) {
        for f in 0..frames {
            for ((samples, [s, h]), w) in planar.iter().zip(self.filters.iter_mut()).zip(&self.weights) {
                let Some(&x) = samples.get(f) else { continue };
                let y = h.run(s.run(x as f64));
                self.acc += w * y * y;
            }
            self.in_step += 1;
            if self.in_step == self.step {
                self.steps.push(self.acc);
                self.acc = 0.0;
                self.in_step = 0;
            }
        }
    }

    /// Integrated loudness in LUFS (None: shorter than one 400 ms block, or silent).
    pub fn integrated(&self) -> Option<f64> {
        let n = 4 * self.step;
        // Each block: the mean square of four steps.
        let blocks: Vec<f64> = self.steps.windows(4).map(|w| w.iter().sum::<f64>() / n as f64).collect();
        let lufs = |z: f64| -0.691 + 10.0 * z.log10();
        let abs: Vec<f64> = blocks.into_iter().filter(|z| *z > 0.0 && lufs(*z) > -70.0).collect();
        if abs.is_empty() {
            return None;
        }
        let rel = lufs(abs.iter().sum::<f64>() / abs.len() as f64) - 10.0;
        let gated: Vec<f64> = abs.into_iter().filter(|z| lufs(*z) > rel).collect();
        (!gated.is_empty()).then(|| lufs(gated.iter().sum::<f64>() / gated.len() as f64))
    }
}

/// The level change (dB) that brings `measured` LUFS to `target` LUFS, kept within OpenQ's level range.
pub fn gain_to(measured: f64, target: f64) -> f32 {
    ((target - measured) as f32).clamp(crate::limits::MIN_DB, crate::limits::MAX_DB)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(rate: u32, secs: f64, amp: f32, freq: f64) -> Vec<f32> {
        (0..(rate as f64 * secs) as usize).map(|i| amp * (std::f64::consts::TAU * freq * i as f64 / rate as f64).sin() as f32).collect()
    }

    /// BS.1770's own figures: the 48 kHz filter coefficients, and a full-scale 997 Hz sine in one
    /// channel reads −3.01 LUFS (at any sample rate).
    #[test]
    fn matches_the_standard() {
        let [s, h] = k_weighting(48000.0);
        for (got, want) in s.b.iter().chain(&s.a).zip([1.53512485958697, -2.69169618940638, 1.19839281085285, -1.69065929318241, 0.73248077421585]) {
            assert!((got - want).abs() < 1e-9, "{got} {want}");
        }
        assert!((h.a[0] + 1.99004745483398).abs() < 1e-9 && (h.a[1] - 0.99007225036621).abs() < 1e-9);
        for rate in [44100, 48000, 96000] {
            let mut m = Meter::new(rate, 2);
            let x = sine(rate, 5.0, 1.0, 997.0);
            m.push(&[x.clone(), vec![0.0; x.len()]], x.len());
            let l = m.integrated().unwrap();
            assert!((l + 3.01).abs() < 0.05, "{rate}: {l}");
        }
    }

    /// Quiet passages under the relative gate don't pull the result down; silence measures nothing.
    #[test]
    fn gating() {
        let rate = 48000;
        let mut loud = sine(rate, 5.0, 0.5, 997.0);
        let quiet = sine(rate, 20.0, 0.005, 997.0);
        let mut m = Meter::new(rate, 1);
        m.push(std::slice::from_ref(&loud), loud.len());
        let alone = m.integrated().unwrap();
        loud.extend(quiet);
        let mut m = Meter::new(rate, 1);
        m.push(std::slice::from_ref(&loud), loud.len());
        // (Within the blocks that straddle the change.)
        assert!((m.integrated().unwrap() - alone).abs() < 0.2, "{} {alone}", m.integrated().unwrap());
        let mut m = Meter::new(rate, 1);
        m.push(&[vec![0.0; 96000]], 96000);
        assert_eq!(m.integrated(), None);
        assert_eq!(gain_to(-14.0, -23.0), -9.0);
    }
}
