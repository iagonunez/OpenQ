//! DMX frames and outputs.
//!
//! The light engine builds one [`DmxFrame`] per tick and hands it to every [`OutputSender`].
//! Each output runs on its own thread with a "latest frame wins" slot, so a slow or stuck
//! device (a USB interface that blocks, a network hiccup) never delays the engine or the other
//! outputs. Senders send universes that changed right away and all of them every
//! [`KEEPALIVE`] (nodes drop their output when nothing arrives). A device that fails is
//! reopened every [`RETRY`].

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Slots in a universe.
pub const SLOTS: usize = 512;
/// Every universe is resent at least this often.
pub const KEEPALIVE: Duration = Duration::from_millis(800);
/// A failed output is reopened after this long.
pub const RETRY: Duration = Duration::from_secs(2);

/// One universe of DMX data.
#[derive(Clone, PartialEq, Debug)]
pub struct Universe {
    /// Universe number (logical in a [`DmxFrame`], wire number inside an output).
    pub id: u16,
    /// Slot values (slot 1 at index 0).
    pub data: [u8; SLOTS],
}

impl Universe {
    /// An empty (all zero) universe.
    pub fn new(id: u16) -> Self {
        Self { id, data: [0; SLOTS] }
    }
}

/// Every universe the light engine drives, sorted by number.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct DmxFrame {
    /// Universes, sorted by `id`.
    pub universes: Vec<Universe>,
}

impl DmxFrame {
    /// A universe by number.
    pub fn get(&self, id: u16) -> Option<&Universe> {
        self.universes.binary_search_by_key(&id, |u| u.id).ok().map(|i| &self.universes[i])
    }

    /// A universe by number, added (zeroed) if missing.
    pub fn get_mut(&mut self, id: u16) -> &mut Universe {
        let i = match self.universes.binary_search_by_key(&id, |u| u.id) {
            Ok(i) => i,
            Err(i) => {
                self.universes.insert(i, Universe::new(id));
                i
            }
        };
        &mut self.universes[i]
    }

    /// Zero every slot (keeps the universes allocated).
    pub fn clear(&mut self) {
        for u in &mut self.universes {
            u.data = [0; SLOTS];
        }
    }
}

/// Sends logical universe `universe` as `wire` on an output (Art-Net port address, sACN universe…).
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub struct UniverseRoute {
    /// Logical universe of the patch (1-based).
    pub universe: u16,
    /// Universe number on the wire.
    pub wire: u16,
}

/// Which protocol/device an output uses.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub enum OutputProtocol {
    /// Art-Net 4 over UDP.
    ArtNet {
        /// Node IP (unicast) or a broadcast address; empty = 255.255.255.255.
        target: String,
    },
    /// sACN (ANSI E1.31) over UDP.
    Sacn {
        /// Unicast IP; empty = multicast.
        target: String,
        /// Source priority (0..=200, default 100).
        priority: u8,
    },
    /// Enttec DMX USB Pro (and compatibles) on a serial port; sends the first route only.
    EnttecPro {
        /// Serial port (`/dev/ttyUSB0`, `COM3`).
        port: String,
    },
}

impl OutputProtocol {
    /// Name shown in the UI.
    pub fn label(&self) -> &'static str {
        match self {
            OutputProtocol::ArtNet { .. } => "Art-Net",
            OutputProtocol::Sacn { .. } => "sACN",
            OutputProtocol::EnttecPro { .. } => "Enttec USB Pro",
        }
    }
}

/// A configured DMX output.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct OutputConfig {
    /// Name shown in the UI.
    pub name: String,
    /// Off = not opened.
    pub enabled: bool,
    /// Protocol and device.
    pub protocol: OutputProtocol,
    /// Universes sent.
    pub routes: Vec<UniverseRoute>,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            name: "Art-Net".into(),
            enabled: true,
            protocol: OutputProtocol::ArtNet { target: String::new() },
            routes: vec![UniverseRoute { universe: 1, wire: 0 }],
        }
    }
}

/// A device or protocol that sends universes. Runs on the output's own thread.
pub trait DmxOutput: Send {
    /// Send one universe (`universe.id` is the wire number).
    fn send(&mut self, universe: &Universe) -> anyhow::Result<()>;
    /// Called after a batch of universes (protocol sync packets).
    fn flush(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Opens the device behind a config.
pub fn open(protocol: &OutputProtocol) -> anyhow::Result<Box<dyn DmxOutput>> {
    Ok(match protocol {
        OutputProtocol::ArtNet { target } => Box::new(super::artnet::ArtNetOutput::open(target)?),
        OutputProtocol::Sacn { target, priority } => Box::new(super::sacn::SacnOutput::open(target, *priority)?),
        OutputProtocol::EnttecPro { port } => Box::new(super::enttec::EnttecPro::open(port)?),
    })
}

/// Health of an output, for the UI.
#[derive(Clone, Default, Debug)]
pub struct OutputStatus {
    /// Last error (cleared by a successful send).
    pub error: Option<String>,
    /// Universe packets sent.
    pub packets: u64,
    /// Device open.
    pub open: bool,
}

/// Reads an output's status (the sender itself lives on the light thread).
#[derive(Clone)]
pub struct OutputHandle {
    /// Output name.
    pub name: String,
    shared: Arc<Shared>,
}

impl OutputHandle {
    /// Current status.
    pub fn status(&self) -> OutputStatus {
        self.shared.status.lock().clone()
    }
}

struct Slot {
    frame: Vec<Universe>,
    fresh: bool,
}

struct Shared {
    slot: Mutex<Slot>,
    status: Mutex<OutputStatus>,
    quit: AtomicBool,
}

/// A running output: a thread that sends the latest frame to one device.
pub struct OutputSender {
    config: OutputConfig,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

type Opener = Box<dyn FnMut() -> anyhow::Result<Box<dyn DmxOutput>> + Send>;

impl OutputSender {
    /// Start sending to the device described by `config`.
    pub fn start(config: OutputConfig) -> Self {
        let protocol = config.protocol.clone();
        Self::with_opener(config, Box::new(move || open(&protocol)))
    }

    /// Start with a custom device factory (tests, virtual outputs).
    pub fn with_opener(config: OutputConfig, opener: Opener) -> Self {
        let shared = Arc::new(Shared {
            slot: Mutex::new(Slot { frame: Vec::new(), fresh: false }),
            status: Mutex::new(OutputStatus::default()),
            quit: AtomicBool::new(false),
        });
        let s = shared.clone();
        let name = format!("openq-dmx-{}", config.protocol.label());
        let thread = std::thread::Builder::new().name(name).spawn(move || run(s, opener)).ok();
        Self { config, shared, thread }
    }

    /// The config it was started with.
    pub fn config(&self) -> &OutputConfig {
        &self.config
    }

    /// Current status.
    pub fn status(&self) -> OutputStatus {
        self.shared.status.lock().clone()
    }

    /// A handle that reads the status from another thread.
    pub fn handle(&self) -> OutputHandle {
        OutputHandle { name: self.config.name.clone(), shared: self.shared.clone() }
    }

    /// Hand over the latest frame. Copies the routed universes into the slot (no allocation once
    /// the slot has its universes) and wakes the thread.
    pub fn publish(&self, frame: &DmxFrame) {
        {
            let mut slot = self.shared.slot.lock();
            slot.frame.resize_with(self.config.routes.len(), || Universe::new(0));
            for (out, route) in slot.frame.iter_mut().zip(&self.config.routes) {
                out.id = route.wire;
                match frame.get(route.universe) {
                    Some(u) => out.data = u.data,
                    None => out.data = [0; SLOTS],
                }
            }
            slot.fresh = true;
        }
        if let Some(t) = &self.thread {
            t.thread().unpark();
        }
    }
}

impl Drop for OutputSender {
    fn drop(&mut self) {
        self.shared.quit.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            t.thread().unpark();
            // A blocked device write must not hang the caller: don't wait more than briefly.
            let deadline = Instant::now() + Duration::from_millis(100);
            while !t.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(2));
            }
            if t.is_finished() {
                let _ = t.join();
            }
        }
    }
}

fn run(shared: Arc<Shared>, mut opener: Opener) {
    let mut device: Option<Box<dyn DmxOutput>> = None;
    let mut next_open = Instant::now();
    // What was last sent, and when everything was last sent.
    let mut sent: Vec<Universe> = Vec::new();
    let mut work: Vec<Universe> = Vec::new();
    let mut last_full = Instant::now() - KEEPALIVE;
    while !shared.quit.load(Ordering::Relaxed) {
        std::thread::park_timeout(Duration::from_millis(100));
        if shared.quit.load(Ordering::Relaxed) {
            break;
        }
        {
            let mut slot = shared.slot.lock();
            if slot.fresh {
                std::mem::swap(&mut slot.frame, &mut work);
                // The slot keeps the previous buffer, so publish doesn't allocate.
                slot.fresh = false;
            }
        }
        if device.is_none() && Instant::now() >= next_open {
            match crate::recovery::catch(&mut opener) {
                Ok(d) => {
                    device = Some(d);
                    let mut st = shared.status.lock();
                    st.open = true;
                    st.error = None;
                    sent.clear();
                }
                Err(e) => {
                    next_open = Instant::now() + RETRY;
                    let mut st = shared.status.lock();
                    if st.error.is_none() {
                        log::warn!("DMX output: {e:#}");
                    }
                    st.open = false;
                    st.error = Some(format!("{e:#}"));
                }
            }
        }
        let Some(dev) = device.as_mut() else { continue };
        let full = last_full.elapsed() >= KEEPALIVE;
        if full {
            last_full = Instant::now();
        }
        let mut n = 0;
        let result = crate::recovery::catch(|| {
            for (i, u) in work.iter().enumerate() {
                if full || sent.get(i) != Some(u) {
                    dev.send(u)?;
                    n += 1;
                }
            }
            if n > 0 {
                dev.flush()?;
            }
            Ok(())
        });
        match result {
            Ok(()) => {
                sent.clone_from(&work);
                if n > 0 {
                    let mut st = shared.status.lock();
                    st.packets += n;
                    st.error = None;
                }
            }
            Err(e) => {
                log::warn!("DMX output lost: {e:#}");
                device = None;
                next_open = Instant::now() + RETRY;
                let mut st = shared.status.lock();
                st.open = false;
                st.error = Some(format!("{e:#}"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    struct Mock(mpsc::Sender<(u16, u8)>);
    impl DmxOutput for Mock {
        fn send(&mut self, u: &Universe) -> anyhow::Result<()> {
            self.0.send((u.id, u.data[0])).unwrap();
            Ok(())
        }
    }

    #[test]
    fn frame_lookup() {
        let mut f = DmxFrame::default();
        f.get_mut(3).data[0] = 7;
        f.get_mut(1);
        assert_eq!(f.universes.iter().map(|u| u.id).collect::<Vec<_>>(), [1, 3]);
        assert_eq!(f.get(3).unwrap().data[0], 7);
        assert!(f.get(2).is_none());
    }

    #[test]
    fn sender_routes_and_skips_unchanged() {
        let (tx, rx) = mpsc::channel();
        let cfg = OutputConfig {
            routes: vec![UniverseRoute { universe: 1, wire: 10 }, UniverseRoute { universe: 2, wire: 11 }],
            ..Default::default()
        };
        let s = OutputSender::with_opener(cfg, Box::new(move || Ok(Box::new(Mock(tx.clone())) as Box<dyn DmxOutput>)));
        let mut f = DmxFrame::default();
        f.get_mut(1).data[0] = 5;
        s.publish(&f);
        let mut got: Vec<_> = (0..2).map(|_| rx.recv_timeout(Duration::from_secs(1)).unwrap()).collect();
        got.sort();
        assert_eq!(got, [(10, 5), (11, 0)]);
        // Only universe 1 changed.
        f.get_mut(1).data[0] = 6;
        s.publish(&f);
        assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), (10, 6));
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());
        assert_eq!(s.status().packets, 3);
    }

    #[test]
    fn failing_device_reports_and_retries() {
        let cfg = OutputConfig::default();
        let s = OutputSender::with_opener(cfg, Box::new(|| anyhow::bail!("no device")));
        s.publish(&DmxFrame::default());
        let t = Instant::now();
        while s.status().error.is_none() && t.elapsed() < Duration::from_secs(1) {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(s.status().error.as_deref(), Some("no device"));
    }
}
