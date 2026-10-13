//! Object audio: sounds placed as objects in a room of speakers.
//!
//! The workspace describes the room once ([`Speaker`]s in `Settings::speakers`, each feeding a
//! device output, positioned in room coordinates). A cue in object mode places one
//! [`AudioObject`] per source channel; the mixer turns positions into output gains with
//! distance-based amplitude panning (DBAP: gain falls 6 dB per doubling of distance, normalised
//! to constant power), folded into the cue's routing matrix. Moving an object (by hand or with
//! a Fade cue) changes the matrix, which the mixer ramps so motion is click-free.
//!
//! Room coordinates: x from −1 (left) to 1 (right), y from −1 (front, the stage end) to 1
//! (back). Speakers may sit outside that square.

use serde::{Deserialize, Serialize};

/// A loudspeaker in the room.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct Speaker {
    /// Label in the room view.
    pub name: String,
    /// Device output it is connected to, from 0.
    pub output: u16,
    /// Position left (−1) to right (1).
    pub x: f32,
    /// Position front (−1) to back (1).
    pub y: f32,
}

impl Default for Speaker {
    fn default() -> Self {
        Self { name: "Speaker".into(), output: 0, x: 0.0, y: -1.0 }
    }
}

/// One sound source in the room: a cue's source channel.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
#[serde(default)]
pub struct AudioObject {
    /// Position left (−1) to right (1).
    pub x: f32,
    /// Position front (−1) to back (1).
    pub y: f32,
    /// 0 = a point at its position, 1 = spread over the whole room.
    pub spread: f32,
}

impl Default for AudioObject {
    fn default() -> Self {
        Self { x: 0.0, y: 0.0, spread: 0.0 }
    }
}

impl AudioObject {
    /// Interpolate towards `to` at progress `t`.
    pub fn lerp(&self, to: &AudioObject, t: f32) -> AudioObject {
        let l = |a: f32, b: f32| a + (b - a) * t;
        AudioObject { x: l(self.x, to.x), y: l(self.y, to.y), spread: l(self.spread, to.spread) }
    }
}

/// A cue's object-audio settings.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
#[serde(default)]
pub struct ObjectAudio {
    /// Route by object position instead of the crosspoint matrix.
    pub enabled: bool,
    /// One object per source channel (channels without one get [`default_object`]).
    pub objects: Vec<AudioObject>,
}

impl ObjectAudio {
    /// Object mode is off and nothing was placed (not worth saving).
    pub fn is_off(&self) -> bool {
        !self.enabled && self.objects.is_empty()
    }

    /// Object of source channel `i` of `src`.
    pub fn object(&self, i: usize, src: usize) -> AudioObject {
        self.objects.get(i).copied().unwrap_or_else(|| default_object(i, src))
    }

    /// Objects for every one of `src` channels.
    pub fn resolved(&self, src: usize) -> Vec<AudioObject> {
        (0..src).map(|i| self.object(i, src)).collect()
    }
}

/// Where channel `i` of `n` starts: spread across the front half, left to right.
pub fn default_object(i: usize, n: usize) -> AudioObject {
    let x = if n <= 1 { 0.0 } else { -0.6 + 1.2 * i as f32 / (n - 1) as f32 };
    AudioObject { x, y: -0.4, spread: 0.0 }
}

/// Objects part-way through a fade (missing ones on either side keep their place).
pub fn lerp_objects(from: &[AudioObject], to: &[AudioObject], t: f32) -> Vec<AudioObject> {
    let t = t.clamp(0.0, 1.0);
    (0..from.len().max(to.len()))
        .map(|i| match (from.get(i), to.get(i)) {
            (Some(a), Some(b)) => a.lerp(b, t),
            (Some(a), None) => *a,
            (None, Some(b)) => *b,
            (None, None) => AudioObject::default(),
        })
        .collect()
}

/// The room used when the workspace defines none: a usual layout for `channels` outputs
/// (stereo pair, quad, 5.1, 7.1; LFE outputs get no speaker), else a ring.
pub fn default_room(channels: usize) -> Vec<Speaker> {
    let s = |name: &str, output: u16, x: f32, y: f32| Speaker { name: name.into(), output, x, y };
    match channels {
        0 => Vec::new(),
        1 => vec![s("C", 0, 0.0, -1.0)],
        2 => vec![s("L", 0, -0.8, -1.0), s("R", 1, 0.8, -1.0)],
        4 => vec![s("FL", 0, -1.0, -1.0), s("FR", 1, 1.0, -1.0), s("RL", 2, -1.0, 1.0), s("RR", 3, 1.0, 1.0)],
        6 => vec![s("L", 0, -0.8, -1.0), s("R", 1, 0.8, -1.0), s("C", 2, 0.0, -1.0), s("Ls", 4, -1.0, 0.8), s("Rs", 5, 1.0, 0.8)],
        8 => vec![
            s("L", 0, -0.8, -1.0),
            s("R", 1, 0.8, -1.0),
            s("C", 2, 0.0, -1.0),
            s("Lb", 4, -0.6, 1.0),
            s("Rb", 5, 0.6, 1.0),
            s("Ls", 6, -1.0, 0.0),
            s("Rs", 7, 1.0, 0.0),
        ],
        n => ring(n),
    }
}

/// `n` speakers evenly round the room, starting front left and going clockwise.
pub fn ring(n: usize) -> Vec<Speaker> {
    (0..n)
        .map(|i| {
            let a = std::f32::consts::TAU * (i as f32 + 0.5) / n as f32 - std::f32::consts::FRAC_PI_2 - std::f32::consts::PI / n as f32;
            Speaker { name: (i + 1).to_string(), output: i as u16, x: a.cos(), y: a.sin() }
        })
        .collect()
}

/// DBAP gain of every speaker for an object (constant power: the squares add up to 1).
pub fn speaker_gains(obj: &AudioObject, speakers: &[Speaker]) -> Vec<f32> {
    // The blur term keeps a point source from collapsing onto a single speaker's distance of 0,
    // and widens the source as spread grows (at 1, every speaker gets nearly the same).
    let blur = 0.08 + 2.0 * obj.spread.clamp(0.0, 1.0);
    let mut g: Vec<f32> = speakers
        .iter()
        .map(|s| {
            let (dx, dy) = (s.x - obj.x, s.y - obj.y);
            1.0 / (dx * dx + dy * dy + blur * blur).sqrt()
        })
        .collect();
    let norm = g.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        g.iter_mut().for_each(|x| *x /= norm);
    }
    g
}

/// Linear routing gains `[src][out]` that place each source channel at its object's position.
pub fn object_matrix(objects: &ObjectAudio, speakers: &[Speaker], src: usize, out: usize) -> Vec<Vec<f32>> {
    (0..src)
        .map(|i| {
            let mut row = vec![0.0; out];
            for (s, g) in speakers.iter().zip(speaker_gains(&objects.object(i, src), speakers)) {
                if let Some(o) = row.get_mut(s.output as usize) {
                    *o += g;
                }
            }
            row
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gains_are_constant_power_and_follow_the_object() {
        let room = default_room(4);
        let front_left = speaker_gains(&AudioObject { x: -1.0, y: -1.0, spread: 0.0 }, &room);
        let power: f32 = front_left.iter().map(|g| g * g).sum();
        assert!((power - 1.0).abs() < 1e-4);
        // On top of a speaker: nearly everything comes out of it.
        assert!(front_left[0] > 0.99, "{front_left:?}");
        // In the middle: all four equal.
        let mid = speaker_gains(&AudioObject::default(), &room);
        assert!(mid.iter().all(|g| (g - 0.5).abs() < 1e-4), "{mid:?}");
        // Spread evens things out.
        let wide = speaker_gains(&AudioObject { x: -1.0, y: -1.0, spread: 1.0 }, &room);
        assert!(wide[0] < 0.7 && wide[3] > 0.3, "{wide:?}");
    }

    #[test]
    fn matrix_routes_to_the_speakers_outputs() {
        // 5.1: output 3 (LFE) has no speaker and gets nothing.
        let room = default_room(6);
        let obj = ObjectAudio { enabled: true, objects: vec![AudioObject { x: 0.0, y: -1.0, spread: 0.0 }] };
        let m = object_matrix(&obj, &room, 1, 6);
        assert_eq!(m[0][3], 0.0);
        assert!(m[0][2] > 0.9, "{m:?}");
        // A missing object gets the default spot.
        let m = object_matrix(&ObjectAudio::default(), &default_room(2), 2, 2);
        assert!(m[0][0] > m[0][1] && m[1][1] > m[1][0]);
    }

    #[test]
    fn rings_go_round() {
        let r = ring(3);
        assert_eq!(r.len(), 3);
        assert!(r.iter().all(|s| ((s.x * s.x + s.y * s.y).sqrt() - 1.0).abs() < 1e-5));
    }
}
