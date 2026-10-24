//! Lists of input devices (audio inputs, cameras, MIDI ports; MIDI outputs for surface feedback) for the inspector and settings.
//! Asking the system can take a moment (cameras especially), so lists load on a background
//! thread the first time they're needed, and again on Refresh.

use crate::media::audio::DeviceEntry;
use parking_lot::Mutex;
use std::sync::Arc;

/// What the system offers right now.
#[derive(Default, Clone)]
pub struct Devices {
    /// Audio inputs (for Mic cues and LTC).
    pub audio_in: Vec<DeviceEntry>,
    /// Cameras: (device string, readable name).
    pub cameras: Vec<(String, String)>,
    /// MIDI input ports (MTC in).
    pub midi_in: Vec<String>,
    /// MIDI output ports (surface feedback).
    pub midi_out: Vec<String>,
    /// The lists have been read at least once.
    pub loaded: bool,
    /// A refresh is running.
    pub loading: bool,
}

/// Device lists shared with the loader thread.
pub type SharedDevices = Arc<Mutex<Devices>>;

/// Load the lists if they never were.
pub fn ensure(d: &SharedDevices, ctx: &egui::Context) {
    let g = d.lock();
    if !g.loaded && !g.loading {
        drop(g);
        refresh(d, ctx);
    }
}

/// Re-read every list in the background.
pub fn refresh(d: &SharedDevices, ctx: &egui::Context) {
    d.lock().loading = true;
    let (d, ctx) = (d.clone(), ctx.clone());
    std::thread::Builder::new()
        .name("opnq-devices".into())
        .spawn(move || {
            let audio_in = crate::media::input::list_input_devices();
            #[cfg(feature = "video")]
            let cameras = crate::media::video::list_cameras();
            #[cfg(not(feature = "video"))]
            let cameras = Vec::new();
            let midi_in = crate::media::tcio::list_midi_inputs();
            let midi_out = crate::surface::list_midi_outputs();
            *d.lock() = Devices { audio_in, cameras, midi_in, midi_out, loaded: true, loading: false };
            ctx.request_repaint();
        })
        .ok();
}
