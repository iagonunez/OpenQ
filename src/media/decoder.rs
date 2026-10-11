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

pub struct DecodeParams {
    /// Sections to play in order (slices / start-end region with loops).
    pub segments: Vec<crate::model::Segment>,
    /// Playback speed (resampling ratio is scaled by this).
    pub rate: f64,
    /// Channels to emit (source channels, padded/truncated). The mixer does the routing.
    pub channels: usize,
}

/// Peak overview of a file for waveform display.
#[derive(Debug, Default)]
pub struct Waveform {
    /// (min, max) of the mixed-down signal per bucket.
    pub peaks: Vec<[f32; 2]>,
    /// Seconds per bucket.
    pub bucket_secs: f64,
    pub duration: f64,
}

impl Waveform {
    /// (min, max) over the time range [t0, t1).
    pub fn range(&self, t0: f64, t1: f64) -> [f32; 2] {
        if self.peaks.is_empty() || self.bucket_secs <= 0.0 {
            return [0.0, 0.0];
        }
        let n = self.peaks.len();
        let i0 = ((t0 / self.bucket_secs).floor().max(0.0) as usize).min(n - 1);
        let i1 = ((t1 / self.bucket_secs).ceil().max(0.0) as usize).clamp(i0 + 1, n);
        self.peaks[i0..i1]
            .iter()
            .fold([0.0f32, 0.0f32], |a, p| [a[0].min(p[0]), a[1].max(p[1])])
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
    })
}

/// Basic facts about an audio file.
#[derive(Clone, Debug, Default)]
pub struct AudioInfo {
    pub duration: Option<f64>,
    pub sample_rate: u32,
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

pub fn probe(path: &Path) -> anyhow::Result<AudioInfo> {
    let o = open(path)?;
    Ok(AudioInfo { duration: o.duration, sample_rate: o.rate, channels: o.channels.unwrap_or(2) })
}

pub fn spawn(path: PathBuf, params: DecodeParams, out_rate: u32, tx: rtrb::Producer<f32>, shared: Arc<VoiceShared>) {
    // With a vamp (slice looping until devamped) keep read-ahead short, so a Devamp is heard
    // within ~0.3 s instead of after the whole buffer.
    let vamp = params.segments.iter().any(|s| s.plays.is_none());
    let channels = params.channels.max(1);
    let fill_limit = if vamp { (out_rate as usize * 3 / 10) * channels } else { usize::MAX };
    let name = format!("openq-dec-{}", path.file_name().map(|f| f.to_string_lossy()).unwrap_or_default());
    let sh = shared.clone();
    let res = std::thread::Builder::new().name(name).spawn(move || {
        let mut sink =
            Sink { tx, shared: sh.clone(), out_channels: channels, resampler: None, scratch: Vec::new(), fill_limit };
        if let Err(e) = run(&path, &params, out_rate, &mut sink)
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
    for seg in &p.segments {
        let mut passes = 0u32;
        loop {
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
        for f in lo..hi {
            for o in 0..out_ch {
                self.scratch.push(planar.get(o).map_or(0.0, |c| c[f]));
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
