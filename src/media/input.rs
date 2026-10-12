//! Live audio inputs: Mic cues and the LTC timecode reader.
//!
//! Each open input is a cpal input stream on its own thread (the stream isn't `Send` on every
//! platform) that hands interleaved f32 frames to a callback. A Mic cue's callback picks its
//! channels, resamples them to the output rate and pushes them into an external mixer voice.
//! The input and output devices run on separate clocks, so the feeder keeps the voice's buffer
//! short: when it has more than [`MIC_LATENCY_SECS`] queued (clock drift, or after a pause),
//! incoming audio is dropped until it drains.

use super::audio::{DeviceEntry, VoiceShared};
use anyhow::{Context, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, InputCallbackInfo, SampleFormat, SizedSample, StreamConfig};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

/// Most audio a Mic cue keeps queued in the mixer before dropping input, in seconds.
pub const MIC_LATENCY_SECS: f64 = 0.03;

/// Input devices of the default host.
pub fn list_input_devices() -> Vec<DeviceEntry> {
    let host = cpal::default_host();
    let Ok(devs) = host.input_devices() else { return Vec::new() };
    super::audio::device_entries(devs, false)
}

/// An open input stream; dropping it closes the stream.
pub struct InputStream {
    stop: Arc<AtomicBool>,
    /// Input sample rate.
    pub rate: u32,
    /// Input channel count.
    pub channels: usize,
    /// Name of the opened device.
    pub device_name: String,
    /// Peak level of the most recent input, as f32 bits (for meters).
    pub peak: Arc<AtomicU32>,
}

impl Drop for InputStream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

impl InputStream {
    /// Peak since the last call (linear, all channels).
    pub fn take_peak(&self) -> f32 {
        f32::from_bits(self.peak.swap(0, Ordering::Relaxed))
    }
}

type OnData = Box<dyn FnMut(&[f32], usize) + Send>;

/// Open `device` (cpal id; None = default input). `on_data(frames, channels)` runs on the audio
/// input thread with interleaved samples.
pub fn open_input(device: Option<&str>, on_data: impl FnMut(&[f32], usize) + Send + 'static) -> anyhow::Result<InputStream> {
    let stop = Arc::new(AtomicBool::new(false));
    let peak = Arc::new(AtomicU32::new(0));
    let (setup_tx, setup_rx) = std::sync::mpsc::channel::<anyhow::Result<(u32, usize, String)>>();
    let (stop2, peak2) = (stop.clone(), peak.clone());
    let wanted = device.map(str::to_string);
    let mut on_data: OnData = Box::new(on_data);
    std::thread::Builder::new().name("openq-audio-in".into()).spawn(move || {
        let res = (|| -> anyhow::Result<(cpal::Stream, u32, usize, String)> {
            let host = cpal::default_host();
            let dev = match &wanted {
                Some(id) => super::audio::find_device(id, false)?,
                None => host.default_input_device().context("no default audio input")?,
            };
            let name = dev.description().map(|d| d.to_string()).unwrap_or_else(|_| "default input".into());
            let supported = dev.default_input_config()?;
            let format = supported.sample_format();
            let config: StreamConfig = supported.into();
            let (rate, ch) = (config.sample_rate, config.channels as usize);
            let peak = peak2.clone();
            let mut cb = move |data: &[f32]| {
                let p = data.iter().fold(0.0f32, |m, s| m.max(s.abs()));
                peak.fetch_max(p.to_bits(), Ordering::Relaxed);
                on_data(data, ch);
            };
            let stream = match format {
                SampleFormat::F32 => build::<f32>(&dev, config, move |d| cb(d))?,
                SampleFormat::I16 => build::<i16>(&dev, config, move |d| cb(d))?,
                SampleFormat::I32 => build::<i32>(&dev, config, move |d| cb(d))?,
                SampleFormat::U16 => build::<u16>(&dev, config, move |d| cb(d))?,
                SampleFormat::F64 => build::<f64>(&dev, config, move |d| cb(d))?,
                f => return Err(anyhow!("unsupported input sample format {f}")),
            };
            stream.play()?;
            Ok((stream, rate, ch, name))
        })();
        match res {
            Ok((stream, rate, ch, name)) => {
                let _ = setup_tx.send(Ok((rate, ch, name)));
                while !stop2.load(Ordering::Acquire) {
                    std::thread::park_timeout(Duration::from_millis(50));
                }
                drop(stream);
            }
            Err(e) => {
                let _ = setup_tx.send(Err(e));
            }
        }
    })?;
    let (rate, channels, device_name) = setup_rx.recv().context("audio input thread died")??;
    log::info!("audio input: {device_name} @ {rate} Hz, {channels} ch");
    Ok(InputStream { stop, rate, channels, device_name, peak })
}

fn build<T>(dev: &cpal::Device, config: StreamConfig, mut f: impl FnMut(&[f32]) + Send + 'static) -> anyhow::Result<cpal::Stream>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let mut buf: Vec<f32> = Vec::new();
    Ok(dev.build_input_stream(
        config,
        move |data: &[T], _: &InputCallbackInfo| {
            buf.clear();
            buf.extend(data.iter().map(|s| <f32 as FromSample<T>>::from_sample_(*s)));
            f(&buf);
        },
        |e| log::error!("audio input error: {e}"),
        None,
    )?)
}

/// Linear-interpolating sample-rate converter for interleaved audio, streaming across chunks.
pub struct Resampler {
    step: f64,
    pos: f64,
    channels: usize,
    prev: Vec<f32>,
}

impl Resampler {
    /// Convert `from` Hz to `to` Hz for `channels` channels.
    pub fn new(from: u32, to: u32, channels: usize) -> Self {
        Self { step: from as f64 / to as f64, pos: 0.0, channels, prev: vec![0.0; channels] }
    }

    /// Resample one chunk, appending output frames to `out`.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let ch = self.channels;
        let frames = input.len() / ch;
        if (self.step - 1.0).abs() < 1e-9 {
            out.extend_from_slice(&input[..frames * ch]);
            return;
        }
        // Frame -1 is the previous chunk's last frame, so interpolation runs across the seam.
        let at = |i: isize, c: usize| if i < 0 { self.prev[c] } else { input[i as usize * ch + c] };
        while self.pos < frames as f64 - 1.0 {
            let i = self.pos.floor() as isize;
            let t = (self.pos - i as f64) as f32;
            for c in 0..ch {
                out.push(at(i, c) + (at(i + 1, c) - at(i, c)) * t);
            }
            self.pos += self.step;
        }
        self.pos -= frames as f64;
        if frames > 0 {
            self.prev.copy_from_slice(&input[(frames - 1) * ch..frames * ch]);
        }
    }
}

/// Feeds a Mic cue's channels into its mixer voice.
pub struct MicFeeder {
    tx: rtrb::Producer<f32>,
    shared: Arc<VoiceShared>,
    first: usize,
    channels: usize,
    resampler: Resampler,
    picked: Vec<f32>,
    out: Vec<f32>,
    /// Most samples to keep queued.
    max_queued: usize,
    /// Output frames still to send (None = until stopped).
    remaining: Option<u64>,
}

impl MicFeeder {
    /// Feed `channels` input channels from `first` on, captured at `in_rate`, to a voice running at
    /// `out_rate`, for `duration` seconds (0 = until stopped).
    pub fn new(
        tx: rtrb::Producer<f32>,
        shared: Arc<VoiceShared>,
        first: usize,
        channels: usize,
        in_rate: u32,
        out_rate: u32,
        duration: f64,
    ) -> Self {
        Self {
            tx,
            shared,
            first,
            channels,
            resampler: Resampler::new(in_rate, out_rate, channels),
            picked: Vec::new(),
            out: Vec::new(),
            max_queued: ((MIC_LATENCY_SECS * out_rate as f64) as usize).max(256) * channels,
            remaining: (duration > 0.0).then_some((duration * out_rate as f64) as u64),
        }
    }

    /// Take one chunk of interleaved input with `in_ch` channels.
    pub fn push(&mut self, data: &[f32], in_ch: usize) {
        if self.shared.stop.load(Ordering::Acquire) || self.shared.eof.load(Ordering::Acquire) {
            return;
        }
        let ch = self.channels;
        self.picked.clear();
        for frame in data.chunks_exact(in_ch.max(1)) {
            for c in 0..ch {
                self.picked.push(frame.get(self.first + c).copied().unwrap_or(0.0));
            }
        }
        self.out.clear();
        self.resampler.process(&self.picked, &mut self.out);
        let mut frames = self.out.len() / ch;
        if let Some(r) = &mut self.remaining {
            frames = frames.min(*r as usize);
            *r -= frames as u64;
        }
        let queued = self.tx.buffer().capacity() - self.tx.slots();
        // Too much queued (drift or a pause): drop this chunk so latency stays low.
        if queued + frames * ch <= self.max_queued + 4 * ch * 64
            && let Ok(chunk) = self.tx.write_chunk_uninit(frames * ch)
        {
            chunk.fill_from_iter(self.out[..frames * ch].iter().copied());
        }
        if self.remaining == Some(0) {
            self.shared.eof.store(true, Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampler_keeps_rate_and_continuity() {
        // 48 kHz → 44.1 kHz in uneven chunks: output length matches the ratio and a ramp stays a ramp.
        let input: Vec<f32> = (0..4800).map(|i| i as f32).collect();
        let mut r = Resampler::new(48_000, 44_100, 1);
        let mut out = Vec::new();
        for chunk in input.chunks(333) {
            r.process(chunk, &mut out);
        }
        assert!((out.len() as i64 - 4410).abs() <= 2, "{}", out.len());
        let step = 48_000.0 / 44_100.0;
        for (k, v) in out.iter().enumerate() {
            assert!((v - k as f32 * step as f32).abs() < 0.01, "{k}: {v}");
        }
    }

    #[test]
    fn mic_feeder_picks_channels_and_limits_latency() {
        let (tx, mut rx) = rtrb::RingBuffer::<f32>::new(48_000);
        let shared = Arc::new(VoiceShared::default());
        // Channel 2 of a 4-channel input, mono out, same rate.
        let mut f = MicFeeder::new(tx, shared, 2, 1, 48_000, 48_000, 0.0);
        let chunk: Vec<f32> = (0..256).flat_map(|i| [0.0, 0.0, i as f32, 9.0]).collect();
        f.push(&chunk, 4);
        assert_eq!(rx.slots(), 256);
        assert_eq!(rx.pop().unwrap(), 0.0);
        assert_eq!(rx.pop().unwrap(), 1.0);
        // Nobody reads: the queue stops growing near the latency limit.
        for _ in 0..100 {
            f.push(&chunk, 4);
        }
        assert!(rx.slots() < (MIC_LATENCY_SECS * 48_000.0) as usize + 1024, "{}", rx.slots());
    }
}
