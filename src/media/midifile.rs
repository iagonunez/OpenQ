//! MIDI File cues: a Standard MIDI File read into a timed list of messages and played out of a
//! MIDI port from a thread of its own.
//!
//! [`parse`] reads formats 0, 1 and 2 (format 2 tracks play one after another), with the tempo
//! map from any track and SMPTE divisions. Files come from the show, so everything is bounded:
//! file size, event count, lengths read from the file are checked against what is left.
//!
//! [`Player`] opens the port and reads the file on its thread (never on the engine thread), sends
//! each message at its time, and silences the notes it left sounding when it is paused, stopped
//! or dropped. Starting part way in sends the latest program, controller and pitch bend of each
//! channel first, so the instruments sound as they would have at that point.

use parking_lot::Mutex;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Largest MIDI file read.
pub const MAX_FILE: u64 = 16 << 20;
/// Most messages kept from one file.
pub const MAX_EVENTS: usize = 4_000_000;

/// One message and when it is sent, in seconds from the start of the file.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Event {
    /// Seconds from the start.
    pub at: f64,
    start: u32,
    len: u32,
}

/// A parsed MIDI file: its messages in time order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Song {
    /// Messages in time order.
    pub events: Vec<Event>,
    data: Vec<u8>,
    /// Seconds to the last message or end-of-track, whichever is later.
    pub duration: f64,
    /// Number of tracks in the file.
    pub tracks: usize,
}

impl Song {
    /// The bytes of `e`.
    pub fn bytes(&self, e: &Event) -> &[u8] {
        &self.data[e.start as usize..(e.start + e.len) as usize]
    }

    /// The first message at or after `at` seconds.
    pub fn index_at(&self, at: f64) -> usize {
        self.events.partition_point(|e| e.at < at)
    }

    /// The program, controller and pitch bend messages that set each channel up before message
    /// `index`: the last of each kind per channel (controller: per number), in time order.
    pub fn chase(&self, index: usize) -> Vec<&[u8]> {
        let mut last: std::collections::HashMap<(u8, u8), usize> = std::collections::HashMap::new();
        for (i, e) in self.events[..index].iter().enumerate() {
            let b = self.bytes(e);
            let key = match b {
                [s, n, _] if s & 0xf0 == 0xb0 && !(120..=127).contains(n) => (*s, *n),
                [s, _] if s & 0xf0 == 0xc0 => (*s, 0),
                [s, _, _] if s & 0xf0 == 0xe0 => (*s, 0),
                _ => continue,
            };
            last.insert(key, i);
        }
        let mut picked: Vec<usize> = last.into_values().collect();
        picked.sort_unstable();
        picked.into_iter().map(|i| self.bytes(&self.events[i])).collect()
    }
}

struct Reader<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn left(&self) -> usize {
        self.b.len() - self.pos
    }
    fn u8(&mut self) -> Result<u8, String> {
        let v = *self.b.get(self.pos).ok_or("MIDI file ends early")?;
        self.pos += 1;
        Ok(v)
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        if n > self.left() {
            return Err("MIDI file ends early".into());
        }
        self.pos += n;
        Ok(&self.b[self.pos - n..self.pos])
    }
    fn u16(&mut self) -> Result<u16, String> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }
    fn u32(&mut self) -> Result<u32, String> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
    /// Variable-length quantity (at most four bytes, 28 bits).
    fn vlq(&mut self) -> Result<u32, String> {
        let mut v = 0u32;
        for _ in 0..4 {
            let b = self.u8()?;
            v = (v << 7) | (b & 0x7f) as u32;
            if b & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err("MIDI file: a length or time is too long".into())
    }
}

/// A message before timing: absolute tick, then the bytes' place in `data`.
struct Raw {
    tick: u64,
    start: u32,
    len: u32,
}

/// Read a Standard MIDI File.
pub fn parse(bytes: &[u8]) -> Result<Song, String> {
    let mut r = Reader { b: bytes, pos: 0 };
    if r.take(4).ok() != Some(b"MThd".as_slice()) {
        return Err("not a MIDI file".into());
    }
    let header_len = r.u32()? as usize;
    if header_len < 6 {
        return Err("MIDI file: header too short".into());
    }
    let format = r.u16()?;
    let ntracks = r.u16()? as usize;
    let division = r.u16()?;
    r.take(header_len - 6)?;
    if format > 2 {
        return Err(format!("MIDI file format {format} is not known"));
    }
    // Seconds per tick for SMPTE divisions; ticks per quarter note otherwise.
    let smpte = (division & 0x8000 != 0).then(|| {
        let fps = match -((division >> 8) as u8 as i8) {
            29 => 30_000.0 / 1001.0,
            f => f as f64,
        };
        let per_frame = (division & 0xff) as f64;
        1.0 / (fps * per_frame).max(1.0)
    });
    let tpq = (division & 0x7fff).max(1) as f64;

    let mut raws: Vec<Raw> = Vec::new();
    let mut data: Vec<u8> = Vec::new();
    let mut tempos: Vec<(u64, u32)> = Vec::new();
    let mut end_tick = 0u64;
    let mut base = 0u64; // format 2: each track starts where the last one ended
    let mut tracks = 0usize;
    while r.left() >= 8 && tracks < ntracks {
        let kind = r.take(4)?;
        let len = r.u32()? as usize;
        if len > r.left() {
            return Err("MIDI file: a track runs past the end".into());
        }
        let body = r.take(len)?;
        if kind != b"MTrk" {
            continue; // unknown chunks are skipped, as the standard says
        }
        tracks += 1;
        let mut t = Reader { b: body, pos: 0 };
        let mut tick = base;
        let mut running: Option<u8> = None;
        while t.left() > 0 {
            tick += t.vlq()? as u64;
            let first = t.u8()?;
            let status = if first & 0x80 != 0 {
                first
            } else {
                t.pos -= 1;
                running.ok_or("MIDI file: data with no status")?
            };
            let mut push = |msg: &[u8], data: &mut Vec<u8>| -> Result<(), String> {
                if raws.len() >= MAX_EVENTS {
                    return Err("MIDI file has too many messages".into());
                }
                raws.push(Raw { tick, start: data.len() as u32, len: msg.len() as u32 });
                data.extend_from_slice(msg);
                Ok(())
            };
            match status {
                0x80..=0xef => {
                    running = Some(status);
                    let n = if matches!(status & 0xf0, 0xc0 | 0xd0) { 1 } else { 2 };
                    let d = t.take(n)?;
                    let msg = [status, d[0] & 0x7f, d.get(1).map_or(0, |b| b & 0x7f)];
                    push(&msg[..1 + n], &mut data)?;
                }
                0xf0 | 0xf7 => {
                    running = None;
                    let len = t.vlq()? as usize;
                    let body = t.take(len)?;
                    let mut msg = Vec::with_capacity(len + 1);
                    if status == 0xf0 {
                        msg.push(0xf0);
                    }
                    msg.extend_from_slice(body);
                    if !msg.is_empty() {
                        push(&msg, &mut data)?;
                    }
                }
                0xff => {
                    running = None;
                    let kind = t.u8()?;
                    let len = t.vlq()? as usize;
                    let body = t.take(len)?;
                    match (kind, body) {
                        (0x51, [a, b, c]) => tempos.push((tick, u32::from_be_bytes([0, *a, *b, *c]).max(1))),
                        (0x2f, _) => break,
                        _ => {}
                    }
                }
                s => return Err(format!("MIDI file: status {s:#04x} is not allowed in a file")),
            }
        }
        end_tick = end_tick.max(tick);
        if format == 2 {
            base = tick;
        }
    }
    if tracks == 0 {
        return Err("MIDI file has no tracks".into());
    }
    raws.sort_by_key(|e| e.tick); // stable: same-tick messages keep the file's order
    tempos.sort_by_key(|t| t.0);

    // Ticks → seconds through the tempo map (500 000 µs a quarter note until the first change).
    let seconds = {
        let mut i = 0usize;
        let (mut at_tick, mut at_secs, mut us) = (0u64, 0.0f64, 500_000u32);
        move |tick: u64| -> f64 {
            if let Some(s) = smpte {
                return tick as f64 * s;
            }
            while i < tempos.len() && tempos[i].0 <= tick {
                at_secs += (tempos[i].0 - at_tick) as f64 * us as f64 / 1e6 / tpq;
                at_tick = tempos[i].0;
                us = tempos[i].1;
                i += 1;
            }
            at_secs + (tick - at_tick) as f64 * us as f64 / 1e6 / tpq
        }
    };
    let mut seconds = seconds;
    let max = crate::limits::MAX_TIME;
    let events: Vec<Event> =
        raws.iter().map(|e| Event { at: seconds(e.tick), start: e.start, len: e.len }).take_while(|e| e.at <= max).collect();
    let duration = seconds(end_tick.max(raws.last().map_or(0, |e| e.tick))).min(max);
    Ok(Song { events, data, duration, tracks })
}

/// Read and parse a MIDI file (a regular local file of at most [`MAX_FILE`]).
pub fn load(path: &std::path::Path) -> Result<Song, String> {
    super::check_local(path).map_err(|e| format!("{e:#}"))?;
    let bytes = crate::recovery::read_file(path, MAX_FILE).map_err(|e| format!("{}: {e}", path.display()))?;
    parse(&bytes)
}

/// Pause/resume requests for the player thread.
#[derive(Default)]
struct Control {
    /// When it was paused.
    paused: Option<Instant>,
    resume: Option<Instant>,
}

/// A running MIDI File cue; dropping it stops it (the thread silences its notes and ends).
pub struct Player {
    stop: Arc<AtomicBool>,
    control: Arc<Mutex<Control>>,
    errors: Arc<Mutex<Vec<String>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// What a [`Player`] plays.
pub struct PlayParams {
    /// The file.
    pub path: PathBuf,
    /// MIDI output port (None = the first one).
    pub port: Option<String>,
    /// Speed (1 = as written).
    pub rate: f64,
    /// Start again at the end, forever.
    pub looped: bool,
    /// Seconds of file time to start at.
    pub offset: f64,
}

impl Player {
    /// Start playing at `at`.
    pub fn start(p: PlayParams, at: Instant) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let control = Arc::new(Mutex::new(Control::default()));
        let errors = Arc::new(Mutex::new(Vec::new()));
        let (s, c, e) = (stop.clone(), control.clone(), errors.clone());
        let thread = std::thread::Builder::new()
            .name("opnq-midi-file".into())
            .spawn(move || {
                if let Err(err) = crate::recovery::catch(|| {
                    play(p, at, &s, &c).map_err(anyhow::Error::msg)
                }) {
                    e.lock().push(format!("MIDI file: {err:#}"));
                }
            })
            .map_err(|err| errors.lock().push(format!("MIDI file: {err}")))
            .ok();
        Self { stop, control, errors, thread }
    }

    /// Hold where it is (sounding notes are released).
    pub fn pause(&self) {
        self.control.lock().paused.get_or_insert_with(Instant::now);
    }

    /// Carry on from `at`.
    pub fn resume(&self, at: Instant) {
        let mut c = self.control.lock();
        if c.paused.is_some() {
            c.resume = Some(at);
        }
    }

    /// Errors since the last call.
    pub fn drain_errors(&self) -> Vec<String> {
        std::mem::take(&mut *self.errors.lock())
    }

    /// The file ended (or couldn't be played).
    pub fn ended(&self) -> bool {
        self.thread.as_ref().is_none_or(|t| t.is_finished())
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        // Never wait on the engine thread: the player sees the flag within a few milliseconds
        // (or once a slow port has opened), releases its notes and ends.
        self.stop.store(true, Ordering::Release);
        self.thread.take();
    }
}

/// Notes left sounding, per channel (bit per note number).
#[derive(Default)]
struct Sounding([u128; 16]);

impl Sounding {
    fn note(&mut self, msg: &[u8]) {
        if let [s, n, v] = *msg {
            let (ch, bit) = ((s & 0x0f) as usize, 1u128 << (n & 0x7f));
            match s & 0xf0 {
                0x90 if v > 0 => self.0[ch] |= bit,
                0x80 | 0x90 => self.0[ch] &= !bit,
                _ => {}
            }
        }
    }

    /// Note offs for every sounding note, then sustain off on those channels.
    fn release(&mut self, send: &mut impl FnMut(&[u8])) {
        for (ch, notes) in self.0.iter_mut().enumerate() {
            if *notes == 0 {
                continue;
            }
            for n in 0..128u8 {
                if *notes & (1u128 << n) != 0 {
                    send(&[0x80 | ch as u8, n, 0]);
                }
            }
            send(&[0xb0 | ch as u8, 64, 0]);
            *notes = 0;
        }
    }
}

fn play(p: PlayParams, at: Instant, stop: &AtomicBool, control: &Mutex<Control>) -> Result<(), String> {
    let (mut conn, _) = super::midiout::connect(p.port.as_deref(), "opnq-midi-file")?;
    let song = load(&p.path)?;
    let rate = p.rate.clamp(crate::limits::RATE.0 as f64, crate::limits::RATE.1 as f64);
    let mut sounding = Sounding::default();
    let mut send = |msg: &[u8]| -> Result<(), String> {
        if !crate::overrides::allowed(crate::overrides::Override::MidiOut) {
            return Ok(());
        }
        conn.send(msg).map_err(|e| e.to_string())
    };
    // File time `from` is heard at `base`.
    let (mut base, mut from) = (at, p.offset.clamp(0.0, song.duration));
    let mut i = song.index_at(from);
    // Waits until `t`; false when stopped or paused meanwhile.
    let wait_until = |t: Instant| loop {
        if stop.load(Ordering::Acquire) || control.lock().paused.is_some() {
            return false;
        }
        let now = Instant::now();
        if now >= t {
            return true;
        }
        std::thread::sleep((t - now).min(Duration::from_millis(2)));
    };
    if !wait_until(base) && stop.load(Ordering::Acquire) {
        return Ok(());
    }
    for msg in song.chase(i) {
        send(msg)?;
    }
    loop {
        if stop.load(Ordering::Acquire) {
            sounding.release(&mut |m| drop(send(m)));
            return Ok(());
        }
        let paused = control.lock().paused;
        if let Some(when) = paused {
            sounding.release(&mut |m| drop(send(m)));
            // File time where it stopped.
            from = (from + when.saturating_duration_since(base).as_secs_f64() * rate).min(song.duration);
            loop {
                if stop.load(Ordering::Acquire) {
                    return Ok(());
                }
                let mut c = control.lock();
                if let Some(r) = c.resume.take() {
                    c.paused = None;
                    base = r;
                    break;
                }
                drop(c);
                std::thread::sleep(Duration::from_millis(5));
            }
            i = song.index_at(from);
            continue;
        }
        let Some(e) = song.events.get(i) else {
            // The end: wait out the last track's end, then loop or finish.
            let end = base + Duration::from_secs_f64(((song.duration - from) / rate).max(0.0));
            if !wait_until(end) {
                continue;
            }
            if !p.looped || song.duration <= 0.0 {
                return Ok(());
            }
            (base, from, i) = (end, 0.0, 0);
            continue;
        };
        let t = base + Duration::from_secs_f64(((e.at - from) / rate).max(0.0));
        if !wait_until(t) {
            continue;
        }
        let msg = song.bytes(e);
        sounding.note(msg);
        if let Err(err) = send(msg) {
            sounding.release(&mut |m| drop(send(m)));
            return Err(err);
        }
        i += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vlq(mut v: u32) -> Vec<u8> {
        let mut out = vec![(v & 0x7f) as u8];
        v >>= 7;
        while v > 0 {
            out.insert(0, (v & 0x7f) as u8 | 0x80);
            v >>= 7;
        }
        out
    }

    fn file(format: u16, division: u16, tracks: &[Vec<u8>]) -> Vec<u8> {
        let mut out = b"MThd".to_vec();
        out.extend(6u32.to_be_bytes());
        out.extend(format.to_be_bytes());
        out.extend((tracks.len() as u16).to_be_bytes());
        out.extend(division.to_be_bytes());
        for t in tracks {
            out.extend(b"MTrk");
            out.extend((t.len() as u32).to_be_bytes());
            out.extend(t);
        }
        out
    }

    fn ev(delta: u32, bytes: &[u8]) -> Vec<u8> {
        let mut v = vlq(delta);
        v.extend(bytes);
        v
    }

    #[test]
    fn tempo_map_running_status_and_tracks_merge() {
        // 480 ticks a quarter; 120 bpm, then 60 bpm from tick 960.
        let mut conductor = ev(0, &[0xff, 0x51, 3, 0x07, 0xa1, 0x20]);
        conductor.extend(ev(960, &[0xff, 0x51, 3, 0x0f, 0x42, 0x40]));
        conductor.extend(ev(0, &[0xff, 0x2f, 0]));
        let mut notes = ev(0, &[0xc0, 5]);
        notes.extend(ev(0, &[0x90, 60, 100]));
        notes.extend(ev(480, &[60, 0])); // running status: note on velocity 0
        notes.extend(ev(960, &[0x90, 62, 90])); // tick 1440: 1 s + a quarter at 60 bpm
        notes.extend(ev(0, &[0xf0, 3, 0x7e, 0x10, 0xf7]));
        notes.extend(ev(480, &[0xff, 0x2f, 0]));
        let song = parse(&file(1, 480, &[conductor, notes])).unwrap();
        let got: Vec<(f64, Vec<u8>)> = song.events.iter().map(|e| (e.at, song.bytes(e).to_vec())).collect();
        assert_eq!(
            got,
            [
                (0.0, vec![0xc0, 5]),
                (0.0, vec![0x90, 60, 100]),
                (0.5, vec![0x90, 60, 0]),
                (2.0, vec![0x90, 62, 90]),
                (2.0, vec![0xf0, 0x7e, 0x10, 0xf7]),
            ]
        );
        assert_eq!(song.duration, 3.0);
        assert_eq!(song.tracks, 2);
        assert_eq!(song.chase(3), [&[0xc0u8, 5][..]], "the program is chased, notes are not");
        assert_eq!(song.index_at(0.5), 2);
    }

    #[test]
    fn smpte_division_and_format_2_tracks_follow_each_other() {
        // 25 fps × 40 ticks a frame = 1 ms a tick.
        let division = (((-25i8) as u8 as u16) << 8) | 40;
        let a = [ev(500, &[0x90, 1, 1]), ev(500, &[0xff, 0x2f, 0])].concat();
        let b = ev(250, &[0x90, 2, 2]);
        let song = parse(&file(2, division, &[a, b])).unwrap();
        let at: Vec<f64> = song.events.iter().map(|e| e.at).collect();
        assert_eq!(at, [0.5, 1.25]);
    }

    #[test]
    fn broken_files_are_errors_not_panics() {
        let good = file(0, 96, &[[ev(0, &[0x90, 1, 1]), ev(10, &[0xff, 0x2f, 0])].concat()]);
        assert!(parse(&good).is_ok());
        for cut in 0..good.len() {
            let _ = parse(&good[..cut]);
        }
        assert!(parse(b"RIFF....").is_err());
        assert!(parse(&file(0, 96, &[vec![0x00, 0x40, 0x7f]])).is_err(), "data with no status");
        assert!(parse(&file(0, 96, &[ev(0, &[0xf0, 0x8f, 0xff, 0xff, 0x7f])])).is_err(), "sysex longer than the track");
        assert!(parse(&file(0, 96, &[vec![0xff, 0xff, 0xff, 0xff, 0x7f]])).is_err(), "five-byte time");
        assert!(parse(&file(3, 96, &[])).is_err());
        let mut rng = fastrand::Rng::with_seed(3);
        for _ in 0..2000 {
            let mut f = good.clone();
            for _ in 0..rng.usize(1..4) {
                let i = rng.usize(..f.len());
                f[i] = rng.u8(..);
            }
            let _ = parse(&f);
        }
    }

    #[test]
    fn released_notes_and_sustain() {
        let mut s = Sounding::default();
        s.note(&[0x91, 60, 100]);
        s.note(&[0x91, 64, 100]);
        s.note(&[0x81, 64, 0]);
        s.note(&[0x92, 10, 1]);
        s.note(&[0x92, 10, 0]);
        let mut sent = Vec::new();
        s.release(&mut |m| sent.push(m.to_vec()));
        assert_eq!(sent, [vec![0x81, 60, 0], vec![0xb1, 64, 0]]);
        s.release(&mut |m| sent.push(m.to_vec()));
        assert_eq!(sent.len(), 2, "nothing left");
    }
}
