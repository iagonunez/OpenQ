//! Remote replicas: another OpenQ on the network connects to this one (the *main device*),
//! mirrors its workspace and live state, and controls it within the role it was given.
//!
//! Security model (the handshake itself is in [`secure`]):
//! * A device pairs once, with a short code shown on the main device while pairing is open.
//!   Pairing stays open for 5 minutes and closes after 5 wrong attempts. The code is checked
//!   with SPAKE2, so someone listening on the network can't test guesses offline: each guess
//!   needs a live connection and uses up one attempt.
//! * Pairing gives the device a random 256-bit token, and later connections authenticate with
//!   that token. Tokens are kept in the per-user config file (mode 0600), never in workspaces.
//! * Both sides prove they know the secret. Everything after the handshake is encrypted and
//!   authenticated with ChaCha20-Poly1305, using separate keys per direction.
//! * The main device gives each paired device a role (view / control / edit) and enforces it on
//!   every message. Removing a device drops its connection at once.
//! * Repeated failures from one address block it for a while.

pub mod client;
pub mod discovery;
pub mod secure;
pub mod server;

use crate::control::Command;
use crate::engine::InstanceSnapshot;
use crate::media::MediaInfo;
use crate::media::decoder::Waveform;
use crate::model::{CueId, Workspace};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

/// TCP port a main device listens on by default.
pub const DEFAULT_PORT: u16 = 53200;
/// UDP port for discovery queries.
pub const DISCOVERY_PORT: u16 = 53201;
/// Wire protocol version; both ends must match.
pub const PROTOCOL: u32 = 1;

/// What a paired device may do.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Role {
    /// Watch the show: cue list, playhead, running cues and meters.
    View,
    /// Also run it: GO, stop, pause, playhead, single cues.
    #[default]
    Control,
    /// Also edit the workspace.
    Edit,
}

impl Role {
    /// Every role, for menus.
    pub const ALL: [Role; 3] = [Role::View, Role::Control, Role::Edit];

    /// Name shown in the UI.
    pub fn label(self) -> &'static str {
        match self {
            Role::View => "View only",
            Role::Control => "Control",
            Role::Edit => "Control & edit",
        }
    }

    /// May run the show (GO, stop, playhead...).
    pub fn can_control(self) -> bool {
        self != Role::View
    }

    /// May change the workspace.
    pub fn can_edit(self) -> bool {
        self == Role::Edit
    }
}

/// A device allowed to connect to this instance.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct TrustedDevice {
    /// The device's own instance id.
    pub id: Uuid,
    /// Name it reported.
    pub name: String,
    /// Its secret login key (hex).
    pub token: String,
    /// What it may do.
    pub role: Role,
    /// Unix seconds.
    pub paired_at: u64,
    /// Last successful login (Unix seconds).
    pub last_seen: u64,
}

/// A main device this instance has paired with.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct KnownServer {
    /// The main device's instance id.
    pub id: Uuid,
    /// Its name.
    pub name: String,
    /// Where it was last reached (`host:port`).
    pub address: String,
    /// Our login key for it.
    pub token: String,
}

/// Per-user remote settings and secrets (not part of any workspace).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct RemoteConfig {
    /// Stable id of this OpenQ installation.
    pub instance_id: Uuid,
    /// Name shown to other devices.
    pub instance_name: String,
    /// Accept connections from paired devices.
    pub server_enabled: bool,
    /// TCP port to listen on.
    pub port: u16,
    /// Role given to newly paired devices.
    pub default_role: Role,
    /// Devices allowed to connect here.
    pub devices: Vec<TrustedDevice>,
    /// Main devices this one has paired with.
    pub servers: Vec<KnownServer>,
}

impl Default for RemoteConfig {
    fn default() -> Self {
        Self {
            instance_id: Uuid::new_v4(),
            instance_name: hostname(),
            server_enabled: false,
            port: DEFAULT_PORT,
            default_role: Role::Control,
            devices: Vec::new(),
            servers: Vec::new(),
        }
    }
}

fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "OpenQ".into())
}

/// Directory for per-user settings (`OPENQ_CONFIG_DIR` overrides it).
pub fn config_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("OPENQ_CONFIG_DIR") {
        return Some(PathBuf::from(d));
    }
    if cfg!(windows) {
        return std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("OpenQ"));
    }
    if cfg!(target_os = "macos") {
        return std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support/OpenQ"));
    }
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .map(|d| d.join("openq"))
}

impl RemoteConfig {
    fn path() -> Option<PathBuf> {
        config_dir().map(|d| d.join("remote.json"))
    }

    /// Load the config, creating (and saving) a fresh identity on first run.
    pub fn load() -> Self {
        let loaded = Self::path().and_then(|p| std::fs::read_to_string(p).ok()).and_then(|s| match serde_json::from_str(&s) {
            Ok(c) => Some(c),
            Err(e) => {
                log::warn!("remote config unreadable, starting fresh: {e}");
                None
            }
        });
        loaded.unwrap_or_else(|| {
            let c = Self::default();
            if let Err(e) = c.save() {
                log::warn!("cannot save remote config: {e:#}");
            }
            c
        })
    }

    /// Write atomically, readable by this user only (it holds device tokens).
    pub fn save(&self) -> anyhow::Result<()> {
        use std::io::Write;
        let path = Self::path().ok_or_else(|| anyhow::anyhow!("no config directory"))?;
        let dir = path.parent().expect("config path has a parent");
        std::fs::create_dir_all(dir)?;
        let tmp = path.with_extension("json.tmp");
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        f.write_all(serde_json::to_string_pretty(self)?.as_bytes())?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }
}

/// Seconds since the Unix epoch.
pub fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Cryptographically random bytes from the OS.
pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::getrandom(&mut b).expect("OS random number generator");
    b
}

/// A new device token: 256 random bits, hex.
pub fn new_token() -> String {
    random_bytes::<32>().iter().map(|b| format!("{b:02x}")).collect()
}

/// Pairing-code alphabet: no 0/O, 1/I/L or U, so codes are easy to read out and type.
const CODE_ALPHABET: &[u8] = b"23456789ABCDEFGHJKMNPQRSTVWXYZ";
/// Symbols in a pairing code.
pub const CODE_LEN: usize = 8;

/// A new pairing code like `7KQM-3XRT` (8 symbols from 30, about 39 bits).
pub fn new_pairing_code() -> String {
    let n = CODE_ALPHABET.len();
    let mut out = String::new();
    // Rejection sampling keeps every symbol equally likely.
    while out.len() < CODE_LEN + 1 {
        for b in random_bytes::<16>() {
            if (b as usize) < 256 - 256 % n && out.len() < CODE_LEN + 1 {
                if out.len() == 4 {
                    out.push('-');
                }
                out.push(CODE_ALPHABET[b as usize % n] as char);
            }
        }
    }
    out
}

/// Canonical form of a typed code: separators and spaces dropped, upper case.
pub fn normalize_code(code: &str) -> String {
    code.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_uppercase()).collect()
}

/// Live engine state sent to replicas.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct LiveState {
    /// Cue GO fires next.
    pub playhead: Option<CueId>,
    /// A Stop-all fade is running.
    pub panicking: bool,
    /// Running cues.
    pub instances: Vec<InstanceSnapshot>,
    /// Engine log.
    pub log: Vec<String>,
    /// Output peaks per channel.
    pub meters: Vec<f32>,
    /// The main device has unsaved changes.
    pub dirty: bool,
}

/// Main device → replica.
#[derive(Serialize, Deserialize, Debug)]
pub enum ServerMsg {
    /// First message after the handshake. `token` is set when this connection paired.
    /// Handshake accepted.
    Welcome {
        /// What the device may do.
        role: Role,
        /// Login key to keep, after a pairing.
        token: Option<String>,
    },
    /// The workspace changed (sent whole).
    Workspace {
        /// The workspace.
        ws: Box<Workspace>,
        /// File name for the title bar.
        title: String,
    },
    /// Live engine state (about 20 times a second while things change, else once a second).
    State(Box<LiveState>),
    /// Durations/details of media files (the files themselves stay on the main device).
    MediaInfo(Vec<(PathBuf, Result<MediaInfo, String>)>),
    /// A requested waveform.
    Waveform(PathBuf, Result<Waveform, String>),
    /// The device's role changed.
    Role(Role),
    /// A request was refused (e.g. not allowed by the role).
    Refused(String),
}

/// Replica → main device.
#[derive(Serialize, Deserialize, Debug)]
pub enum ClientMsg {
    /// Run a show-control command.
    Command(Command),
    /// The whole workspace after a local edit (Edit role only).
    Edit(Box<Workspace>),
    /// Ask for a media file's waveform.
    Waveform(PathBuf),
    /// Keep-alive.
    Ping,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_and_tokens() {
        for _ in 0..50 {
            let c = new_pairing_code();
            assert_eq!(c.len(), CODE_LEN + 1);
            assert_eq!(&c[4..5], "-");
            assert!(normalize_code(&c).bytes().all(|b| CODE_ALPHABET.contains(&b)));
        }
        assert_eq!(normalize_code(" 7kqm-3xrt "), "7KQM3XRT");
        assert_ne!(new_token(), new_token());
        assert_eq!(new_token().len(), 64);
    }
}
