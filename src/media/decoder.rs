//! Per-voice decoder thread: symphonia decode → trim/loop → resample → channel map → ring buffer.

use super::audio::VoiceShared;
use anyhow::{Context, anyhow};
use rubato::audioadapter_buffers::direct::SequentialSliceOfVecs;
use rubato::{Async, Fft, FixedAsync, FixedSync, Indexing, Resampler, SincInterpolationParameters};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::{Time, TimeBase};

/// How a decoder voice reads its file.
pub struct DecodeParams {
    /// Sections to play in order (slices / start-end region with loops).
    pub segments: Vec<crate::model::Segment>,
    /// Playback speed (resampling ratio is scaled by this).
    pub rate: f64,
    /// Channels to emit (source channels, padded/truncated). The mixer does the routing.
    pub channels: usize,
    /// Integrated fade-in at the start, in seconds of output.
    pub fade_in: f64,
    /// Integrated fade-out before the end, in seconds of output (only when the end is known).
    pub fade_out: f64,
    /// Shape of both fades.
    pub fade_curve: crate::model::FadeCurve,
}

/// Peak overview of a file for waveform display.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Waveform {
    /// (min, max) of the mixed-down signal per bucket.
    pub peaks: Vec<[f32; 2]>,
    /// Seconds per bucket.
    pub bucket_secs: f64,
    /// File duration in seconds.
    pub duration: f64,
    /// Coarser copies of `peaks` (level k merges 2^(k+1) buckets), made on first use, so a
    /// zoomed-out view of a long file doesn't scan every bucket per column.
    #[serde(skip)]
    levels: std::sync::OnceLock<Vec<Vec<[f32; 2]>>>,
}

fn merge(a: [f32; 2], b: [f32; 2]) -> [f32; 2] {
    [a[0].min(b[0]), a[1].max(b[1])]
}

impl Waveform {
    /// A waveform from its peaks.
    pub fn new(peaks: Vec<[f32; 2]>, bucket_secs: f64, duration: f64) -> Self {
        Self { peaks, bucket_secs, duration, levels: Default::default() }
    }

    /// (min, max) over the time range [t0, t1).
    pub fn range(&self, t0: f64, t1: f64) -> [f32; 2] {
        if self.peaks.is_empty() || self.bucket_secs <= 0.0 {
            return [0.0, 0.0];
        }
        let n = self.peaks.len();
        let mut i = ((t0 / self.bucket_secs).floor().max(0.0) as usize).min(n - 1);
        let end = ((t1 / self.bucket_secs).ceil().max(0.0) as usize).clamp(i + 1, n);
        let levels = self.levels.get_or_init(|| {
            let mut out: Vec<Vec<[f32; 2]>> = Vec::new();
            let mut cur = &self.peaks;
            while cur.len() > 1 {
                let next: Vec<[f32; 2]> = cur.chunks(2).map(|c| c.iter().copied().fold(c[0], merge)).collect();
                out.push(next);
                cur = out.last().unwrap();
            }
            out
        });
        // Exact: cover [i, end) with the largest aligned blocks (O(log n) instead of O(n)).
        let mut acc = [0.0f32, 0.0f32];
        while i < end {
            let mut k = 0;
            while k < levels.len() && i.is_multiple_of(2 << k) && i + (2 << k) <= end {
                k += 1;
            }
            acc = merge(acc, if k == 0 { self.peaks[i] } else { levels[k - 1][i >> k] });
            i += 1 << k;
        }
        acc
    }
}

/// Decode the whole file into a 10 ms peak overview.
pub fn waveform(path: &Path) -> anyhow::Result<Waveform> {
    let mut o = open(path)?;
    let per_bucket = (o.rate as usize / 100).max(1);
    let mut peaks = Vec::new();
    let mut cur = [0f32, 0f32];
    let mut in_bucket = 0usize;
    let mut total = 0u64;
    let mut planar: Vec<Vec<f32>> = Vec::new();
    loop {
        let packet = match o.format.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break,
            Err(SymError::ResetRequired) => {
                o.decoder.reset();
                continue;
            }
            Err(SymError::IoError(_)) => break,
            Err(e) => return Err(anyhow!(e)),
        };
        if packet.track_id != o.track_id {
            continue;
        }
        let buf = match o.decoder.decode(&packet) {
            Ok(b) => b,
            Err(SymError::DecodeError(_)) | Err(SymError::IoError(_)) => continue,
            Err(e) => return Err(anyhow!(e)),
        };
        let frames = buf.frames();
        buf.copy_to_vecs_planar::<f32>(&mut planar);
        for f in 0..frames {
            for ch in planar.iter() {
                let v = ch[f];
                cur[0] = cur[0].min(v);
                cur[1] = cur[1].max(v);
            }
            in_bucket += 1;
            if in_bucket == per_bucket {
                peaks.push(cur);
                cur = [0.0, 0.0];
                in_bucket = 0;
            }
        }
        total += frames as u64;
    }
    if in_bucket > 0 {
        peaks.push(cur);
    }
    Ok(Waveform {
        peaks,
        bucket_secs: per_bucket as f64 / o.rate as f64,
        duration: total as f64 / o.rate as f64,
        ..Default::default()
    })
}

/// Basic facts about an audio file.
#[derive(Clone, Debug, Default)]
pub struct AudioInfo {
    /// Duration in seconds, if the container says.
    pub duration: Option<f64>,
    /// Native sample rate.
    pub sample_rate: u32,
    /// Channel count.
    pub channels: usize,
}

struct Opened {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn AudioDecoder>,
    track_id: u32,
    time_base: TimeBase,
    rate: u32,
    channels: Option<usize>,
    duration: Option<f64>,
}

fn open(path: &Path) -> anyhow::Result<Opened> {
    let file = std::fs::File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let format = symphonia::default::get_probe()
        .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
        .context("unsupported audio format")?;
    let track = format.default_track(TrackType::Audio).context("no audio track")?;
    let params = track
        .codec_params
        .as_ref()
        .and_then(|p| p.audio())
        .context("missing audio codec parameters")?;
    let rate = params.sample_rate.context("unknown sample rate")?;
    let channels = params.channels.as_ref().map(|c| c.count());
    let time_base = track.time_base.or_else(|| TimeBase::try_from_recip(rate)).context("no time base")?;
    let duration = track
        .num_frames
        .map(|n| n as f64 / rate as f64)
        .or_else(|| track.duration.and_then(|d| time_base.calc_duration(d)).map(|t| t.as_secs_f64()));
    let decoder = symphonia::default::get_codecs()
        .make_audio_decoder(params, &AudioDecoderOptions::default())
        .context("unsupported codec")?;
    let track_id = track.id;
    Ok(Opened { format, decoder, track_id, time_base, rate, channels, duration })
}

/// Read an audio file's duration, sample rate and channels.
pub fn probe(path: &Path) -> anyhow::Result<AudioInfo> {
    let o = open(path)?;
    Ok(AudioInfo { duration: o.duration, sample_rate: o.rate, channels: o.channels.unwrap_or(2) })
}

/// Start a decoder thread for one voice: decodes `params.segments` of `path`, resamples to
/// `out_rate` and pushes interleaved samples into `tx` until done or stopped via `shared`.
pub fn spawn(path: PathBuf, params: DecodeParams, out_rate: u32, tx: rtrb::Producer<f32>, shared: Arc<VoiceShared>) {
    // With a vamp (slice looping until devamped) keep read-ahead short, so a Devamp is heard
    // within ~0.3 s instead of after the whole buffer.
    let vamp = params.segments.iter().any(|s| s.plays.is_none());
    let channels = params.channels.max(1);
    let fill_limit = if vamp { (out_rate as usize * 3 / 10) * channels } else { usize::MAX };
    let name = format!("openq-dec-{}", path.file_name().map(|f| f.to_string_lossy()).unwrap_or_default());
    let sh = shared.clone();
    let res = std::thread::Builder::new().name(name).spawn(move || {
        let mut sink = Sink {
            tx,
            shared: sh.clone(),
            out_channels: channels,
            resampler: None,
            scratch: Vec::new(),
            fill_limit,
            envelope: Envelope {
                written: 0,
                fade_in: (params.fade_in.max(0.0) * out_rate as f64) as u64,
                fade_out: None,
                curve: params.fade_curve,
            },
        };
        let res = crate::recovery::catch(|| {
            crate::recovery::fault_point(crate::recovery::Fault::Decoder);
            run(&path, &params, out_rate, &mut sink)
        });
        if let Err(e) = res
            && e.downcast_ref::<Stopped>().is_none()
        {
            log::error!("decode {}: {e:#}", path.display());
            *sh.error.lock() = Some(format!("{e:#}"));
        }
        sh.ready.store(true, Ordering::Release);
        sh.eof.store(true, Ordering::Release);
    });
    if let Err(e) = res {
        *shared.error.lock() = Some(format!("cannot spawn decoder: {e}"));
        shared.eof.store(true, Ordering::Release);
    }
}

#[derive(Debug)]
struct Stopped;
impl std::fmt::Display for Stopped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("stopped")
    }
}
impl std::error::Error for Stopped {}

fn run(path: &Path, p: &DecodeParams, out_rate: u32, sink: &mut Sink) -> anyhow::Result<()> {
    let rate = if p.rate.is_finite() && p.rate > 0.0 { p.rate.clamp(0.05, 8.0) } else { 1.0 };
    let file_len = if p.fade_out > 0.0 { open(path).ok().and_then(|o| o.duration) } else { None };
    for (si, seg) in p.segments.iter().enumerate() {
        let mut passes = 0u32;
        loop {
            // Once the rest of the playback has a known length, place the integrated fade-out.
            if sink.envelope.fade_out.is_none()
                && let Some(len) = file_len
                && let Some(rest) = remaining_file_time(&p.segments[si..], passes, len)
            {
                let out_frames = (rest / rate * out_rate as f64) as u64;
                let fade = ((p.fade_out * out_rate as f64) as u64).min(out_frames);
                sink.envelope.fade_out = Some((sink.envelope.written + out_frames - fade, fade));
            }
            play_pass(path, seg.start, seg.end, rate, out_rate, sink)?;
            passes += 1;
            if sink.shared.stop.load(Ordering::Acquire) {
                return Err(Stopped.into());
            }
            // A devamp lets the current slice finish this pass, then moves on.
            if sink.shared.devamp.swap(false, Ordering::AcqRel) {
                break;
            }
            if seg.plays.is_some_and(|n| passes >= n) {
                break;
            }
        }
    }
    sink.finish()
}

/// File time left to play from the start of pass `done` of `segs[0]` through the end, if that is
/// known (None while a pass count is unlimited).
fn remaining_file_time(segs: &[crate::model::Segment], done: u32, file_len: f64) -> Option<f64> {
    let mut total = 0.0;
    for (i, s) in segs.iter().enumerate() {
        let len = (s.end.unwrap_or(file_len).min(file_len) - s.start).max(0.0);
        let plays = s.plays? as f64 - if i == 0 { done as f64 } else { 0.0 };
        total += len * plays.max(1.0);
    }
    Some(total)
}

/// Integrated fade-in / fade-out applied to the decoded output.
struct Envelope {
    /// Output frames written so far.
    written: u64,
    /// Fade-in length in output frames.
    fade_in: u64,
    /// Fade-out (first frame, length) in output frames, once the end is known.
    fade_out: Option<(u64, u64)>,
    curve: crate::model::FadeCurve,
}

impl Envelope {
    /// Gain for the next output frame (and advance).
    fn next(&mut self) -> f32 {
        let n = self.written;
        self.written += 1;
        let mut g = 1.0;
        if n < self.fade_in {
            g *= self.curve.gain(n as f32 / self.fade_in as f32);
        }
        if let Some((start, len)) = self.fade_out
            && n >= start
        {
            let t = if len == 0 { 1.0 } else { (n - start) as f32 / len as f32 };
            g *= self.curve.gain(1.0 - t);
        }
        g
    }

    fn active(&self) -> bool {
        self.written < self.fade_in || self.fade_out.is_some()
    }
}

/// Decode [start, end) once into the sink.
fn play_pass(path: &Path, start: f64, end: Option<f64>, rate: f64, out_rate: u32, sink: &mut Sink) -> anyhow::Result<()> {
    // Re-open for every pass: robust for formats with poor seeking, and cheap.
    let mut o = open(path)?;
    if sink.resampler.is_none() && (o.rate != out_rate || (rate - 1.0).abs() > 1e-6) {
        sink.resampler = Some(Resample::new(o.rate, out_rate, rate, sink.out_channels)?);
    }
    if start > 0.0 {
        let time = Time::try_from_secs_f64(start).context("bad start time")?;
        if let Err(e) = o.format.seek(SeekMode::Accurate, SeekTo::Time { time, track_id: Some(o.track_id) }) {
            log::warn!("seek failed ({e}); decoding from file start");
        }
        o.decoder.reset();
    }
    let mut planar: Vec<Vec<f32>> = Vec::new();
    loop {
        if sink.shared.stop.load(Ordering::Acquire) {
            return Err(Stopped.into());
        }
        let packet = match o.format.next_packet() {
            Ok(Some(pk)) => pk,
            Ok(None) => break,
            Err(SymError::ResetRequired) => {
                o.decoder.reset();
                continue;
            }
            Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(anyhow!(e)),
        };
        if packet.track_id != o.track_id {
            continue;
        }
        let pkt_time = o.time_base.calc_time_saturating(packet.pts).as_secs_f64();
        if let Some(end) = end
            && pkt_time >= end
        {
            break;
        }
        let buf = match o.decoder.decode(&packet) {
            Ok(b) => b,
            Err(SymError::DecodeError(e)) => {
                log::debug!("decode error (skipped): {e}");
                continue;
            }
            Err(SymError::IoError(_)) => continue,
            Err(e) => return Err(anyhow!(e)),
        };
        let frames = buf.frames();
        if frames == 0 {
            continue;
        }
        buf.copy_to_vecs_planar::<f32>(&mut planar);
        let r = o.rate as f64;
        // Trim to [start, end).
        let lo = (((start - pkt_time) * r).round().max(0.0) as usize).min(frames);
        let hi = match end {
            Some(end) => (((end - pkt_time) * r).round().max(0.0) as usize).min(frames),
            None => frames,
        };
        if hi > lo {
            sink.push(&planar, lo, hi)?;
        }
    }
    Ok(())
}

struct Resample {
    rs: Box<dyn Resampler<f32>>,
    channels: usize,
    acc: Vec<Vec<f32>>,
    out: Vec<Vec<f32>>,
    delay_left: usize,
    in_total: u64,
    out_total: u64,
    ratio: f64,
}

impl Resample {
    fn new(from: u32, to: u32, speed: f64, channels: usize) -> anyhow::Result<Self> {
        let ratio = to as f64 / (from as f64 * speed);
        let rs: Box<dyn Resampler<f32>> = if (speed - 1.0).abs() < 1e-6 {
            // Fixed rational ratio: FFT resampler is fastest and cleanest.
            Box::new(
                Fft::<f32>::new(from as usize, to as usize, 1024, channels, FixedSync::Input)
                    .map_err(|e| anyhow!("resampler: {e}"))?,
            )
        } else {
            Box::new(
                Async::<f32>::new_sinc(ratio, 1.1, &SincInterpolationParameters::default(), 1024, channels, FixedAsync::Input)
                    .map_err(|e| anyhow!("resampler: {e}"))?,
            )
        };
        let out_max = rs.output_frames_max();
        Ok(Self {
            delay_left: rs.output_delay(),
            rs,
            channels,
            acc: vec![Vec::new(); channels],
            out: vec![vec![0.0; out_max]; channels],
            in_total: 0,
            out_total: 0,
            ratio,
        })
    }
}

struct Sink {
    tx: rtrb::Producer<f32>,
    shared: Arc<VoiceShared>,
    out_channels: usize,
    resampler: Option<Resample>,
    scratch: Vec<f32>,
    /// Most samples to keep buffered ahead of playback.
    fill_limit: usize,
    envelope: Envelope,
}

impl Sink {
    fn push(&mut self, planar: &[Vec<f32>], lo: usize, hi: usize) -> anyhow::Result<()> {
        let Some(mut r) = self.resampler.take() else {
            return self.write(planar, lo, hi);
        };
        let res = (|| {
            for c in 0..r.channels {
                // Missing source channels (layout changed mid-stream) are filled with silence.
                match planar.get(c) {
                    Some(src) => r.acc[c].extend_from_slice(&src[lo..hi]),
                    None => r.acc[c].extend(std::iter::repeat_n(0.0, hi - lo)),
                }
            }
            r.in_total += (hi - lo) as u64;
            while r.acc[0].len() >= r.rs.input_frames_next() {
                self.process_chunk(&mut r, None)?;
            }
            Ok(())
        })();
        self.resampler = Some(r);
        res
    }

    fn process_chunk(&mut self, r: &mut Resample, partial: Option<usize>) -> anyhow::Result<()> {
        let avail = r.acc[0].len();
        let input = SequentialSliceOfVecs::new(&r.acc, r.channels, avail).map_err(|e| anyhow!("{e:?}"))?;
        let out_len = r.out[0].len();
        let mut output =
            SequentialSliceOfVecs::new_mut(&mut r.out, r.channels, out_len).map_err(|e| anyhow!("{e:?}"))?;
        let mut idx = Indexing::new();
        idx.partial_len = partial;
        let (nin, nout) = r
            .rs
            .process_into_buffer(&input, &mut output, Some(&idx))
            .map_err(|e| anyhow!("resample: {e}"))?;
        let nin = nin.min(avail);
        for a in r.acc.iter_mut() {
            a.drain(..nin);
        }
        // Drop the resampler's startup delay so playback starts exactly at the start point,
        // and never emit more than the input length implies.
        let skip = r.delay_left.min(nout);
        r.delay_left -= skip;
        let expected = (r.in_total as f64 * r.ratio).round() as u64;
        let room = expected.saturating_sub(r.out_total) as usize;
        let n = (nout - skip).min(room);
        if n > 0 {
            let out = std::mem::take(&mut r.out);
            let res = self.write(&out, skip, skip + n);
            r.out = out;
            res?;
            r.out_total += n as u64;
        }
        Ok(())
    }

    fn finish(&mut self) -> anyhow::Result<()> {
        if let Some(mut r) = self.resampler.take() {
            let expected = (r.in_total as f64 * r.ratio).round() as u64;
            let mut guard = 0;
            while r.out_total < expected && guard < 64 {
                let left = r.acc[0].len();
                self.process_chunk(&mut r, Some(left))?;
                guard += 1;
            }
        }
        Ok(())
    }

    /// Interleave frames [lo, hi) as `out_channels` channels (pad with silence / truncate) and
    /// push them into the ring buffer (blocking).
    fn write(&mut self, planar: &[Vec<f32>], lo: usize, hi: usize) -> anyhow::Result<()> {
        let out_ch = self.out_channels;
        if planar.is_empty() {
            return Ok(());
        }
        self.scratch.clear();
        self.scratch.reserve((hi - lo) * out_ch);
        if self.envelope.active() {
            for f in lo..hi {
                let g = self.envelope.next();
                for o in 0..out_ch {
                    self.scratch.push(planar.get(o).map_or(0.0, |c| c[f]) * g);
                }
            }
        } else {
            self.envelope.written += (hi - lo) as u64;
            for f in lo..hi {
                for o in 0..out_ch {
                    self.scratch.push(planar.get(o).map_or(0.0, |c| c[f]));
                }
            }
        }
        let mut off = 0;
        while off < self.scratch.len() {
            if self.shared.stop.load(Ordering::Acquire) {
                return Err(Stopped.into());
            }
            let used = self.tx.buffer().capacity() - self.tx.slots();
            let room = self.fill_limit.saturating_sub(used);
            let n = self.tx.slots().min(room).min(self.scratch.len() - off);
            if n == 0 {
                self.shared.ready.store(true, Ordering::Release);
                std::thread::sleep(Duration::from_millis(5));
                continue;
            }
            let mut chunk = self.tx.write_chunk(n).map_err(|e| anyhow!("{e}"))?;
            let (a, b) = chunk.as_mut_slices();
            let la = a.len();
            a.copy_from_slice(&self.scratch[off..off + la]);
            b.copy_from_slice(&self.scratch[off + la..off + n]);
            chunk.commit_all();
            off += n;
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::model::{FadeCurve, Segment};

    /// A mono 16-bit WAV of a full-scale square-ish signal (constant ±0.5).
    pub(crate) fn write_wav(path: &Path, rate: u32, secs: f32) {
        let n = (rate as f32 * secs) as u32;
        let mut b = Vec::new();
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&(36 + n * 2).to_le_bytes());
        b.extend_from_slice(b"WAVEfmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes()); // PCM
        b.extend_from_slice(&1u16.to_le_bytes()); // mono
        b.extend_from_slice(&rate.to_le_bytes());
        b.extend_from_slice(&(rate * 2).to_le_bytes());
        b.extend_from_slice(&2u16.to_le_bytes());
        b.extend_from_slice(&16u16.to_le_bytes());
        b.extend_from_slice(b"data");
        b.extend_from_slice(&(n * 2).to_le_bytes());
        for i in 0..n {
            let v: i16 = if (i / 50) % 2 == 0 { 16384 } else { -16384 };
            b.extend_from_slice(&v.to_le_bytes());
        }
        std::fs::write(path, b).unwrap();
    }

    fn decode_all(path: &Path, params: DecodeParams, rate: u32) -> Vec<f32> {
        let (tx, mut rx) = rtrb::RingBuffer::<f32>::new(rate as usize * 4);
        let shared = Arc::new(VoiceShared::default());
        spawn(path.to_path_buf(), params, rate, tx, shared.clone());
        let mut out = Vec::new();
        let t0 = std::time::Instant::now();
        while !(shared.eof.load(Ordering::Acquire) && rx.slots() == 0) && t0.elapsed() < Duration::from_secs(10) {
            while let Ok(s) = rx.pop() {
                out.push(s);
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        out
    }

    fn peak(s: &[f32]) -> f32 {
        s.iter().fold(0.0f32, |m, v| m.max(v.abs()))
    }

    #[test]
    fn integrated_fades_shape_start_and_end() {
        let path = std::env::temp_dir().join(format!("openq-fade-{}.wav", std::process::id()));
        write_wav(&path, 48000, 2.0);
        let seg = |plays| vec![Segment { start: 0.0, end: None, plays }];
        let params = |fade_in, fade_out, plays| DecodeParams {
            segments: seg(plays),
            rate: 1.0,
            channels: 1,
            fade_in,
            fade_out,
            fade_curve: FadeCurve::Linear,
        };
        let out = decode_all(&path, params(0.5, 0.5, Some(1)), 48000);
        assert!((out.len() as i64 - 96000).abs() < 100, "{} samples", out.len());
        assert!(peak(&out[..480]) < 0.02, "start not silent: {}", peak(&out[..480]));
        assert!((peak(&out[11500..12500]) - 0.25).abs() < 0.02, "quarter-way: {}", peak(&out[11500..12500]));
        assert!((peak(&out[40000..56000]) - 0.5).abs() < 0.01, "middle not full level");
        assert!(peak(&out[out.len() - 480..]) < 0.02, "end not silent");
        // Two plays: the fade-out is on the second pass only.
        let out = decode_all(&path, params(0.0, 0.5, Some(2)), 48000);
        assert!((peak(&out[94000..98000]) - 0.5).abs() < 0.01, "faded at the loop point");
        assert!(peak(&out[out.len() - 480..]) < 0.02);
        std::fs::remove_file(path).unwrap();
    }

    /// Range queries through the coarse levels give exactly what scanning every bucket does.
    #[test]
    fn waveform_range_matches_a_full_scan() {
        let mut seed = 12345u32;
        let mut rnd = || {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            (seed >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0
        };
        for n in [1usize, 2, 3, 7, 64, 1000, 4097] {
            let peaks: Vec<[f32; 2]> = (0..n).map(|_| { let (a, b) = (rnd(), rnd()); [a.min(b), a.max(b)] }).collect();
            let w = Waveform { peaks: peaks.clone(), bucket_secs: 0.01, duration: n as f64 * 0.01, ..Default::default() };
            for _ in 0..200 {
                let t0 = (rnd() + 1.0) as f64 * 0.5 * w.duration;
                let t1 = t0 + (rnd() + 1.0) as f64 * 0.5 * w.duration;
                let i0 = ((t0 / 0.01).floor() as usize).min(n - 1);
                let i1 = ((t1 / 0.01).ceil() as usize).clamp(i0 + 1, n);
                let want = peaks[i0..i1].iter().fold([0.0f32, 0.0], |a, p| [a[0].min(p[0]), a[1].max(p[1])]);
                assert_eq!(w.range(t0, t1), want, "n {n} [{t0}, {t1})");
            }
        }
    }
}
