//! Per-voice decoder thread: symphonia decode → trim/loop → resample → channel map → ring buffer.

use super::audio::VoiceShared;
use anyhow::{Context, anyhow};
use rubato::audioadapter_buffers::direct::SequentialSliceOfVecs;
use rubato::{Fft, FixedSync, Indexing, Resampler};
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
    pub start: f64,
    pub end: Option<f64>,
    /// None = loop forever.
    pub plays: Option<u32>,
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

pub fn spawn(
    path: PathBuf,
    params: DecodeParams,
    out_rate: u32,
    out_channels: usize,
    tx: rtrb::Producer<f32>,
    shared: Arc<VoiceShared>,
) {
    let name = format!("openq-dec-{}", path.file_name().map(|f| f.to_string_lossy()).unwrap_or_default());
    let sh = shared.clone();
    let res = std::thread::Builder::new().name(name).spawn(move || {
        let mut sink = Sink { tx, shared: sh.clone(), out_channels, resampler: None, scratch: Vec::new() };
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
    let mut plays_done = 0u32;
    loop {
        // Re-open for every pass: robust for formats with poor seeking, and cheap.
        let mut o = open(path)?;
        if sink.resampler.is_none() && o.rate != out_rate {
            let ch = o.channels.unwrap_or(2);
            sink.resampler = Some(Resample::new(o.rate, out_rate, ch)?);
        }
        if p.start > 0.0 {
            let time = Time::try_from_secs_f64(p.start).context("bad start time")?;
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
            let rate = o.rate as f64;
            // Trim to [start, end).
            let lo = (((p.start - pkt_time) * rate).round().max(0.0) as usize).min(frames);
            let hi = match p.end {
                Some(end) => (((end - pkt_time) * rate).round().max(0.0) as usize).min(frames),
                None => frames,
            };
            if let Some(end) = p.end
                && pkt_time >= end
            {
                break;
            }
            if hi > lo {
                sink.push(&planar, lo, hi)?;
            }
        }
        plays_done += 1;
        if p.plays.is_some_and(|n| plays_done >= n) {
            break;
        }
        if sink.shared.stop.load(Ordering::Acquire) {
            return Err(Stopped.into());
        }
    }
    sink.finish()
}

struct Resample {
    rs: Fft<f32>,
    channels: usize,
    acc: Vec<Vec<f32>>,
    out: Vec<Vec<f32>>,
    delay_left: usize,
    in_total: u64,
    out_total: u64,
    ratio: f64,
}

impl Resample {
    fn new(from: u32, to: u32, channels: usize) -> anyhow::Result<Self> {
        let rs = Fft::<f32>::new(from as usize, to as usize, 1024, channels, FixedSync::Input)
            .map_err(|e| anyhow!("resampler: {e}"))?;
        let out_max = rs.output_frames_max();
        Ok(Self {
            delay_left: rs.output_delay(),
            rs,
            channels,
            acc: vec![Vec::new(); channels],
            out: vec![vec![0.0; out_max]; channels],
            in_total: 0,
            out_total: 0,
            ratio: to as f64 / from as f64,
        })
    }
}

struct Sink {
    tx: rtrb::Producer<f32>,
    shared: Arc<VoiceShared>,
    out_channels: usize,
    resampler: Option<Resample>,
    scratch: Vec<f32>,
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

    /// Map channels, interleave and push frames [lo, hi) into the ring buffer (blocking).
    fn write(&mut self, planar: &[Vec<f32>], lo: usize, hi: usize) -> anyhow::Result<()> {
        let src_ch = planar.len();
        let out_ch = self.out_channels;
        if src_ch == 0 {
            return Ok(());
        }
        self.scratch.clear();
        self.scratch.reserve((hi - lo) * out_ch);
        for f in lo..hi {
            for o in 0..out_ch {
                let s = if out_ch == 1 {
                    planar.iter().map(|c| c[f]).sum::<f32>() / src_ch as f32
                } else if src_ch == 1 {
                    if o < 2 { planar[0][f] } else { 0.0 }
                } else if o < src_ch {
                    planar[o][f]
                } else {
                    0.0
                };
                self.scratch.push(s);
            }
        }
        let mut off = 0;
        while off < self.scratch.len() {
            if self.shared.stop.load(Ordering::Acquire) {
                return Err(Stopped.into());
            }
            let n = self.tx.slots().min(self.scratch.len() - off);
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
