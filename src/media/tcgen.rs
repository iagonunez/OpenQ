//! Timecode cues: LTC into a mixer voice, MTC out of a MIDI port, each from a thread of its own.
//!
//! LTC: a generator thread keeps the voice's queue filled with [`LtcEncoder`] samples. The mixer
//! plays them at the device's rate, so sample `n` is heard `n` samples after the voice started
//! whatever the queue holds; pausing the voice pauses the code (the full queue stops the thread).
//!
//! MTC: quarter-frame messages, four per frame, eight describing the frame where the first one was
//! sent; a full-frame message at the start and after every resume (receivers locate on it).

use crate::model::TimecodeRate;
use crate::timecode::{LtcEncoder, Timecode, mtc_full_frame, mtc_quarter_frame};
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// A running Timecode cue's generator; dropping it stops it.
pub struct TcGen {
    stop: Arc<AtomicBool>,
    paused: Arc<Mutex<Option<Instant>>>,
    resume: Arc<Mutex<Option<Instant>>>,
    errors: Arc<Mutex<Vec<String>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl TcGen {
    fn spawn(name: &str, f: impl FnOnce(Arc<AtomicBool>, Arc<Mutex<Option<Instant>>>, Arc<Mutex<Option<Instant>>>, Arc<Mutex<Vec<String>>>) + Send + 'static) -> Self {
        let (stop, paused, resume, errors) = (Arc::new(AtomicBool::new(false)), Arc::new(Mutex::new(None)), Arc::new(Mutex::new(None)), Arc::new(Mutex::new(Vec::new())));
        let (s, p, r, e) = (stop.clone(), paused.clone(), resume.clone(), errors.clone());
        let e2 = errors.clone();
        let thread = std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                if let Err(err) = crate::recovery::catch(|| {
                    f(s, p, r, e);
                    Ok(())
                }) {
                    e2.lock().push(format!("timecode: {err:#}"));
                }
            })
            .map_err(|err| errors.lock().push(format!("timecode: {err}")))
            .ok();
        Self { stop, paused, resume, errors, thread }
    }

    /// Feed LTC starting at `start` (label-seconds) into a voice's queue.
    pub fn ltc(mut tx: rtrb::Producer<f32>, start: f64, rate: TimecodeRate, sample_rate: u32, amplitude: f32) -> Self {
        Self::spawn("openq-ltc-out", move |stop, _, _, _| {
            let mut enc = LtcEncoder::new(start, rate, sample_rate, amplitude);
            let mut buf = vec![0.0f32; 256];
            while !stop.load(Ordering::Acquire) {
                let n = tx.slots().min(buf.len());
                if n == 0 {
                    std::thread::sleep(Duration::from_millis(2));
                    continue;
                }
                enc.fill(&mut buf[..n]);
                if let Ok(chunk) = tx.write_chunk_uninit(n) {
                    chunk.fill_from_iter(buf[..n].iter().copied());
                }
            }
        })
    }

    /// Send MTC starting at `start` (label-seconds) from `at`, out of `port` (None = the first
    /// MIDI output).
    pub fn mtc(port: Option<String>, start: f64, rate: TimecodeRate, at: Instant) -> Self {
        Self::spawn("openq-mtc-out", move |stop, paused, resume, errors| {
            let (mut conn, _) = match super::midiout::connect(port.as_deref(), "openq-mtc") {
                Ok(c) => c,
                Err(e) => {
                    errors.lock().push(format!("MTC: {e}"));
                    return;
                }
            };
            let quarter = Duration::from_secs_f64(1.0 / (rate.fps() * 4.0));
            let mut frame = Timecode::from_secs(start, rate).frame_count(rate);
            let mut base = at;
            let mut piece: u8 = 0;
            let mut full = true;
            let wait_until = |t: Instant, stop: &AtomicBool| {
                while !stop.load(Ordering::Acquire) {
                    let now = Instant::now();
                    if now >= t {
                        return;
                    }
                    std::thread::sleep((t - now).min(Duration::from_millis(5)));
                }
            };
            let mut sent: u64 = 0;
            wait_until(base, &stop);
            while !stop.load(Ordering::Acquire) {
                if paused.lock().is_some() {
                    std::thread::sleep(Duration::from_millis(5));
                    if let Some(r) = resume.lock().take() {
                        *paused.lock() = None;
                        base = r;
                        sent = 0;
                        piece = 0;
                        full = true;
                        wait_until(base, &stop);
                    }
                    continue;
                }
                let tc = Timecode::from_frame_count(frame, rate);
                if full {
                    full = false;
                    if let Err(e) = conn.send(&mtc_full_frame(tc, rate)) {
                        errors.lock().push(format!("MTC: {e}"));
                        return;
                    }
                }
                if let Err(e) = conn.send(&mtc_quarter_frame(tc, rate, piece)) {
                    errors.lock().push(format!("MTC: {e}"));
                    return;
                }
                piece += 1;
                if piece == 8 {
                    piece = 0;
                    frame += 2;
                }
                sent += 1;
                wait_until(base + quarter * sent as u32, &stop);
            }
        })
    }

    /// Hold MTC where it is (LTC pauses with its voice).
    pub fn pause(&self) {
        *self.paused.lock() = Some(Instant::now());
    }

    /// Carry on from `at`.
    pub fn resume(&self, at: Instant) {
        if self.paused.lock().is_some() {
            *self.resume.lock() = Some(at);
        }
    }

    /// Errors since the last call.
    pub fn drain_errors(&self) -> Vec<String> {
        std::mem::take(&mut *self.errors.lock())
    }

    /// The thread ended (MTC couldn't open its port).
    pub fn ended(&self) -> bool {
        self.thread.as_ref().is_none_or(|t| t.is_finished())
    }
}

impl Drop for TcGen {
    fn drop(&mut self) {
        // Never wait on the engine thread: the thread sees the flag within a few milliseconds
        // (or once a slow MIDI port has opened) and ends by itself.
        self.stop.store(true, Ordering::Release);
        self.thread.take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timecode::LtcDecoder;

    /// The LTC thread fills the queue with code that decodes to the start time onwards.
    #[test]
    fn ltc_decodes_from_the_start_time() {
        let (tx, mut rx) = rtrb::RingBuffer::<f32>::new(48_000);
        let g = TcGen::ltc(tx, 3600.0, TimecodeRate::Fps25, 48_000, 0.5);
        let mut samples = Vec::new();
        let until = Instant::now() + Duration::from_secs(5);
        while samples.len() < 48_000 && Instant::now() < until {
            while let Ok(v) = rx.pop() {
                samples.push(v);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        drop(g);
        let mut dec = LtcDecoder::new(48_000);
        let got: Vec<String> = dec.push(&samples).into_iter().map(|(tc, _, _)| tc.format(TimecodeRate::Fps25)).collect();
        assert!(got.len() >= 20, "{got:?}");
        assert_eq!(got[0], "01:00:00:00");
        assert!(got.windows(2).all(|w| w[0] < w[1]), "running forward");
    }
}
