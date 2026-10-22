//! Masks for layers and outputs ([`crate::model::Mask`]): pictures read on a thread into one byte
//! per pixel (luminance × alpha), shared by every layer and window that uses the same file.
//!
//! The renderer asks [`cache`] for a file every frame: the first ask starts the load and answers
//! [`MaskState::Loading`] (the layer is hidden until it is ready, so it never flashes unmasked);
//! files nobody asked for in a while are dropped.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

/// Longest side a mask is kept at (a mask needs no more detail than this).
pub const MAX_SIDE: u32 = 4096;
/// A mask no layer or output used for this long is dropped (and read again if used again).
const UNUSED: Duration = Duration::from_secs(10);

/// A loaded mask: one byte per pixel, rows top to bottom.
#[derive(Debug)]
pub struct MaskImage {
    /// Width and height.
    pub size: [u32; 2],
    /// Luminance × alpha, 0..255.
    pub data: Vec<u8>,
}

/// Where a mask's load is.
#[derive(Clone, Debug)]
pub enum MaskState {
    /// Being read.
    Loading,
    /// Ready to use.
    Ready(Arc<MaskImage>),
    /// Couldn't be read: why.
    Failed(Arc<str>),
}

struct Entry {
    state: Arc<Mutex<MaskState>>,
    used: Instant,
}

/// The masks read so far (see the module docs).
#[derive(Default)]
pub struct MaskCache {
    entries: Mutex<HashMap<PathBuf, Entry>>,
}

/// The process's mask cache.
pub fn cache() -> &'static MaskCache {
    static CACHE: OnceLock<MaskCache> = OnceLock::new();
    CACHE.get_or_init(MaskCache::default)
}

impl MaskCache {
    /// The mask in `path`, starting its load on first use.
    pub fn get(&self, path: &Path) -> MaskState {
        let now = Instant::now();
        let mut entries = self.entries.lock();
        entries.retain(|_, e| now.duration_since(e.used) < UNUSED);
        if let Some(e) = entries.get_mut(path) {
            e.used = now;
            return e.state.lock().clone();
        }
        let state = Arc::new(Mutex::new(MaskState::Loading));
        let (s, p) = (state.clone(), path.to_path_buf());
        let spawned = std::thread::Builder::new().name("openq-mask".into()).spawn(move || {
            let r = crate::recovery::catch(|| load(&p));
            *s.lock() = match r {
                Ok(m) => MaskState::Ready(Arc::new(m)),
                Err(e) => MaskState::Failed(format!("mask {}: {e:#}", p.display()).into()),
            };
        });
        if let Err(e) = spawned {
            *state.lock() = MaskState::Failed(format!("mask: cannot start the loader: {e}").into());
        }
        entries.insert(path.to_path_buf(), Entry { state, used: now });
        MaskState::Loading
    }
}

/// Read a mask picture (any still OpenQ shows, SVG included).
pub fn load(path: &Path) -> anyhow::Result<MaskImage> {
    let s = super::still::load(path, 1.0)?;
    let (mut w, mut h, mut rgba) = (s.width, s.height, s.rgba);
    if w.max(h) > MAX_SIDE {
        let img = image::RgbaImage::from_raw(w, h, rgba).ok_or_else(|| anyhow::anyhow!("bad picture size"))?;
        let img = image::imageops::thumbnail(&img, MAX_SIDE, MAX_SIDE);
        (w, h) = img.dimensions();
        rgba = img.into_raw();
    }
    Ok(MaskImage { size: [w, h], data: values(&rgba) })
}

/// Straight RGBA → luminance × alpha (BT.709 weights).
pub fn values(rgba: &[u8]) -> Vec<u8> {
    rgba.as_chunks::<4>()
        .0
        .iter()
        .map(|&[r, g, b, a]| {
            let l = 0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32;
            (l * a as f32 / 255.0).round().min(255.0) as u8
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_are_luminance_times_alpha() {
        assert_eq!(values(&[255, 255, 255, 255, 0, 0, 0, 255, 255, 255, 255, 0, 255, 255, 255, 128]), [255, 0, 0, 128]);
    }

    #[test]
    fn masks_load_once_in_the_background() {
        let dir = std::env::temp_dir().join(format!("openq-mask-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("m.png");
        image::RgbaImage::from_fn(4, 2, |x, _| if x < 2 { image::Rgba([255; 4]) } else { image::Rgba([0, 0, 0, 255]) }).save(&path).unwrap();
        let c = MaskCache::default();
        assert!(matches!(c.get(&path), MaskState::Loading));
        let t = Instant::now();
        let img = loop {
            match c.get(&path) {
                MaskState::Ready(m) => break m,
                MaskState::Failed(e) => panic!("{e}"),
                MaskState::Loading => {}
            }
            assert!(t.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(2));
        };
        assert_eq!(img.size, [4, 2]);
        assert_eq!(img.data, [255, 255, 0, 0, 255, 255, 0, 0]);
        assert!(matches!(c.get(&dir.join("missing.png")), MaskState::Loading));
        let t = Instant::now();
        while !matches!(c.get(&dir.join("missing.png")), MaskState::Failed(_)) {
            assert!(t.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(2));
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
