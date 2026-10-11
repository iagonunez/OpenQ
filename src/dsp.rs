//! Audio DSP helpers: RBJ-cookbook biquads for the per-cue equaliser.

use crate::model::{Eq, EqBand, EqKind, MAX_EQ_BANDS};

/// Normalised biquad coefficients (a0 = 1).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Coeffs {
    /// Feed-forward coefficient for `x[n]`.
    pub b0: f32,
    /// Feed-forward coefficient for `x[n-1]`.
    pub b1: f32,
    /// Feed-forward coefficient for `x[n-2]`.
    pub b2: f32,
    /// Feedback coefficient for `y[n-1]`.
    pub a1: f32,
    /// Feedback coefficient for `y[n-2]`.
    pub a2: f32,
}

impl Default for Coeffs {
    fn default() -> Self {
        Self::IDENTITY
    }
}

impl Coeffs {
    /// Pass-through filter.
    pub const IDENTITY: Coeffs = Coeffs { b0: 1.0, b1: 0.0, b2: 0.0, a1: 0.0, a2: 0.0 };

    /// RBJ-cookbook coefficients for one EQ band at sample rate `rate`.
    pub fn design(band: &EqBand, rate: f32) -> Coeffs {
        let f = band.freq.clamp(10.0, rate * 0.49);
        let q = band.q.max(0.05);
        let w0 = std::f32::consts::TAU * f / rate;
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * q);
        let a = 10f32.powf(band.gain_db / 40.0);
        let sa = 2.0 * a.sqrt() * alpha;
        let (b0, b1, b2, a0, a1, a2) = match band.kind {
            EqKind::Peak => (1.0 + alpha * a, -2.0 * cos, 1.0 - alpha * a, 1.0 + alpha / a, -2.0 * cos, 1.0 - alpha / a),
            EqKind::LowShelf => (
                a * ((a + 1.0) - (a - 1.0) * cos + sa),
                2.0 * a * ((a - 1.0) - (a + 1.0) * cos),
                a * ((a + 1.0) - (a - 1.0) * cos - sa),
                (a + 1.0) + (a - 1.0) * cos + sa,
                -2.0 * ((a - 1.0) + (a + 1.0) * cos),
                (a + 1.0) + (a - 1.0) * cos - sa,
            ),
            EqKind::HighShelf => (
                a * ((a + 1.0) + (a - 1.0) * cos + sa),
                -2.0 * a * ((a - 1.0) + (a + 1.0) * cos),
                a * ((a + 1.0) + (a - 1.0) * cos - sa),
                (a + 1.0) - (a - 1.0) * cos + sa,
                2.0 * ((a - 1.0) - (a + 1.0) * cos),
                (a + 1.0) - (a - 1.0) * cos - sa,
            ),
            EqKind::LowPass => {
                let b = (1.0 - cos) / 2.0;
                (b, 1.0 - cos, b, 1.0 + alpha, -2.0 * cos, 1.0 - alpha)
            }
            EqKind::HighPass => {
                let b = (1.0 + cos) / 2.0;
                (b, -(1.0 + cos), b, 1.0 + alpha, -2.0 * cos, 1.0 - alpha)
            }
        };
        Coeffs { b0: b0 / a0, b1: b1 / a0, b2: b2 / a0, a1: a1 / a0, a2: a2 / a0 }
    }

    /// Magnitude response in dB at `freq`.
    pub fn magnitude_db(&self, freq: f32, rate: f32) -> f32 {
        let w = std::f64::consts::TAU * freq as f64 / rate as f64;
        let (s1, c1) = w.sin_cos();
        let (s2, c2) = (2.0 * w).sin_cos();
        let (b0, b1, b2) = (self.b0 as f64, self.b1 as f64, self.b2 as f64);
        let (a1, a2) = (self.a1 as f64, self.a2 as f64);
        // z^-1 = e^{-jw}
        let nr = b0 + b1 * c1 + b2 * c2;
        let ni = -(b1 * s1 + b2 * s2);
        let dr = 1.0 + a1 * c1 + a2 * c2;
        let di = -(a1 * s1 + a2 * s2);
        let mag2 = (nr * nr + ni * ni) / (dr * dr + di * di).max(1e-30);
        (10.0 * mag2.max(1e-30).log10()) as f32
    }
}

/// Coefficients for the active bands of an EQ (count in `.1`; zero when the EQ is bypassed).
pub fn eq_coeffs(eq: &Eq, rate: f32) -> ([Coeffs; MAX_EQ_BANDS], usize) {
    let mut out = [Coeffs::IDENTITY; MAX_EQ_BANDS];
    let mut n = 0;
    if eq.enabled {
        for b in eq.bands.iter().filter(|b| b.enabled).take(MAX_EQ_BANDS) {
            out[n] = Coeffs::design(b, rate);
            n += 1;
        }
    }
    (out, n)
}

/// Combined response of the whole EQ in dB.
pub fn eq_response_db(eq: &Eq, freq: f32, rate: f32) -> f32 {
    let (c, n) = eq_coeffs(eq, rate);
    c[..n].iter().map(|c| c.magnitude_db(freq, rate)).sum()
}

/// Transposed direct form II state.
#[derive(Clone, Copy, Default)]
pub struct BiquadState {
    s1: f32,
    s2: f32,
}

impl BiquadState {
    /// Filter one sample.
    #[inline]
    pub fn process(&mut self, c: &Coeffs, x: f32) -> f32 {
        let y = c.b0 * x + self.s1;
        self.s1 = c.b1 * x - c.a1 * y + self.s2;
        self.s2 = c.b2 * x - c.a2 * y;
        // Flush denormals so silent tails don't hit the slow path.
        if self.s1.abs() < 1e-20 {
            self.s1 = 0.0;
        }
        if self.s2.abs() < 1e-20 {
            self.s2 = 0.0;
        }
        y
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn band(kind: EqKind, freq: f32, gain_db: f32) -> EqBand {
        EqBand { enabled: true, kind, freq, gain_db, q: 0.707 }
    }

    #[test]
    fn peak_hits_gain_at_centre() {
        let c = Coeffs::design(&EqBand { q: 1.0, ..band(EqKind::Peak, 1000.0, 6.0) }, 48000.0);
        assert!((c.magnitude_db(1000.0, 48000.0) - 6.0).abs() < 0.05);
        assert!(c.magnitude_db(50.0, 48000.0).abs() < 0.3);
    }

    #[test]
    fn shelves_and_filters() {
        let ls = Coeffs::design(&band(EqKind::LowShelf, 200.0, -9.0), 48000.0);
        assert!((ls.magnitude_db(20.0, 48000.0) + 9.0).abs() < 0.3);
        assert!(ls.magnitude_db(10000.0, 48000.0).abs() < 0.3);
        let hp = Coeffs::design(&band(EqKind::HighPass, 1000.0, 0.0), 48000.0);
        assert!(hp.magnitude_db(100.0, 48000.0) < -30.0);
        assert!((hp.magnitude_db(1000.0, 48000.0) + 3.0).abs() < 0.2);
        let lp = Coeffs::design(&band(EqKind::LowPass, 1000.0, 0.0), 48000.0);
        assert!(lp.magnitude_db(10000.0, 48000.0) < -30.0);
    }

    #[test]
    fn processing_matches_response() {
        // A 1 kHz sine through a +6 dB peak comes out ~2x louder.
        let rate = 48000.0;
        let c = Coeffs::design(&EqBand { q: 1.0, ..band(EqKind::Peak, 1000.0, 6.0) }, rate);
        let mut st = BiquadState::default();
        let mut peak = 0f32;
        for i in 0..48000 {
            let x = (i as f32 * std::f32::consts::TAU * 1000.0 / rate).sin();
            let y = st.process(&c, x);
            if i > 24000 {
                peak = peak.max(y.abs());
            }
        }
        assert!((peak - 1.995).abs() < 0.02, "{peak}");
    }

    #[test]
    fn bypassed_eq_has_no_bands() {
        let eq = Eq::default();
        assert_eq!(eq_coeffs(&eq, 48000.0).1, 0);
        let eq = Eq { enabled: true, ..Eq::default() };
        assert_eq!(eq_coeffs(&eq, 48000.0).1, 4);
        assert!(eq_response_db(&eq, 1000.0, 48000.0).abs() < 0.01);
    }
}
