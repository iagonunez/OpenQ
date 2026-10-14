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
use crate::model::{Cue, CueId, Workspace};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

/// TCP port a main device listens on by default.
pub const DEFAULT_PORT: u16 = 53200;
/// UDP port for discovery queries.
pub const DISCOVERY_PORT: u16 = 53201;
/// Wire protocol version; both ends must match.
pub const PROTOCOL: u32 = 3;

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
            Role::View => crate::i18n::t("View only"),
            Role::Control => crate::i18n::t("Control"),
            Role::Edit => crate::i18n::t("Control & edit"),
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
    /// Lighting masters and sequence states.
    #[serde(default)]
    pub lighting: LightLive,
}

/// Live lighting state mirrored to replicas (their executors show the main device's).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct LightLive {
    /// Grand master, blackout, sequence levels, tempo.
    pub masters: crate::light::engine::Masters,
    /// What each sequence is doing.
    pub sequences: Vec<crate::light::playback::SequenceStatus>,
    /// Fixtures selected in the main device's programmer (a visualiser highlights them).
    pub selection: Vec<crate::light::model::FixtureId>,
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
        /// `ws.lighting` is left out: keep the lighting design of the last workspace (a
        /// [`ServerMsg::Lighting`] follows if it changed). A big design isn't resent whenever a
        /// cue is added.
        #[serde(default)]
        keep_lighting: bool,
    },
    /// Only these cues changed since the last workspace sent (their own fields; a group's
    /// children come as separate entries). Sent instead of [`ServerMsg::Workspace`] when the
    /// tree's shape and everything else stayed the same, e.g. while a slider is dragged.
    Cues(Vec<Cue>),
    /// Only these lighting design changes since the last workspace sent (sent with or instead of
    /// [`ServerMsg::Cues`]).
    Lighting(Box<crate::light::patch::LightingPatch>),
    /// DMX output universes that changed (number, 512 slots), about 20 times a second, to a
    /// replica that asked with [`ClientMsg::Subscribe`].
    Dmx(Vec<(u16, Vec<u8>)>),
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
    /// A local edit that changed the cue list's shape: the whole workspace except the lighting
    /// design (`ws.lighting` is ignored), whose changes come as a patch (Edit role only).
    Reshape {
        /// The workspace (its lighting is ignored).
        ws: Box<Workspace>,
        /// Lighting design changes.
        lighting: Option<Box<crate::light::patch::LightingPatch>>,
    },
    /// A local edit as changes to the last workspace received (Edit role only): see
    /// [`workspace_patch`].
    Patch {
        /// Changed cues.
        cues: Vec<Cue>,
        /// Lighting design changes.
        lighting: Option<Box<crate::light::patch::LightingPatch>>,
    },
    /// Ask for a media file's waveform.
    Waveform(PathBuf),
    /// Start or stop receiving the DMX output ([`ServerMsg::Dmx`]): a DMX monitor or a
    /// visualiser.
    Subscribe {
        /// Wanted.
        dmx: bool,
    },
    /// Select fixtures in the main device's programmer (a visualiser picking fixtures; Control
    /// role or above).
    Select(Vec<crate::light::model::FixtureId>),
    /// Send the whole workspace again (a [`ServerMsg::Cues`] didn't fit this replica's copy).
    Resync,
    /// Keep-alive.
    Ping,
}

/// The cues of `new` that differ from `old` (shallow copies), if the two differ only in cues'
/// own fields: same lists, same ids in the same places, same settings and stages. None means
/// the whole workspace has to go.
pub fn cue_patch(old: &Workspace, new: &Workspace) -> Option<Vec<Cue>> {
    fn walk(a: &[Cue], b: &[Cue], out: &mut Vec<Cue>) -> bool {
        a.len() == b.len()
            && a.iter().zip(b).all(|(x, y)| {
                if x.id != y.id || x.is_group() != y.is_group() {
                    return false;
                }
                if !x.shallow_eq(y) {
                    out.push(y.shallow_clone());
                }
                walk(x.children(), y.children(), out)
            })
    }
    let (a, b) = (&old.cue_list, &new.cue_list);
    let same_rest = old.format_version == new.format_version
        && old.settings == new.settings
        && old.stages == new.stages
        && a.info == b.info
        && a.pos == b.pos
        && a.others.len() == b.others.len()
        && a.others.iter().zip(&b.others).all(|(x, y)| x.info == y.info);
    if !same_rest {
        return None;
    }
    let mut out = Vec::new();
    let lists = std::iter::once((&a.cues, &b.cues)).chain(a.others.iter().zip(&b.others).map(|(x, y)| (&x.cues, &y.cues)));
    for (x, y) in lists {
        if !walk(x, y, &mut out) {
            return None;
        }
    }
    Some(out)
}

/// The changes from `old` to `new` as changed cues ([`cue_patch`]) plus a lighting design patch,
/// or None if the whole workspace has to go.
pub fn workspace_patch(old: &Workspace, new: &Workspace) -> Option<(Vec<Cue>, Option<crate::light::patch::LightingPatch>)> {
    Some((cue_patch(old, new)?, crate::light::patch::diff(&old.lighting, &new.lighting)))
}

/// Apply a [`workspace_patch`] to a copy of the workspace it was made from. False (workspace
/// untouched) if it doesn't fit.
pub fn apply_workspace_patch(ws: &mut Workspace, cues: &[Cue], lighting: Option<&crate::light::patch::LightingPatch>) -> bool {
    let mut lights = None;
    if let Some(p) = lighting {
        let mut l = ws.lighting.clone();
        if !crate::light::patch::apply(&mut l, p) {
            return false;
        }
        lights = Some(l);
    }
    // A patch that half fits must leave nothing applied.
    let backup = ws.cue_list.clone();
    if !apply_cue_patch(ws, cues) {
        ws.cue_list = backup;
        return false;
    }
    if let Some(l) = lights {
        ws.lighting = l;
    }
    true
}

/// Apply a [`cue_patch`]. False if a cue isn't there (the copy has drifted; ask for a resync).
pub fn apply_cue_patch(ws: &mut Workspace, patch: &[Cue]) -> bool {
    let mut ok = true;
    for c in patch {
        match ws.cue_list.find_mut(c.id) {
            Some(cue) if cue.is_group() == c.is_group() => cue.assign_shallow(c.clone()),
            _ => ok = false,
        }
    }
    ok
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Edits that keep the tree's shape go as the changed cues only, and applying them gives the
    /// same workspace; anything else needs the whole workspace.
    #[test]
    fn cue_patches() {
        use crate::model::{CueKind, GroupMode};
        let mut ws = Workspace::default();
        let child = Cue::new(CueKind::Memo);
        let group = Cue::new(CueKind::Group { mode: GroupMode::default(), children: vec![child.clone()] });
        ws.cue_list.cues = vec![Cue::new(CueKind::Memo), group.clone()];
        let base = ws.clone();

        let mut edited = ws.clone();
        edited.cue_list.find_mut(child.id).unwrap().name = "renamed".into();
        edited.cue_list.find_mut(group.id).unwrap().pre_wait = 2.0;
        let patch = cue_patch(&base, &edited).expect("same shape");
        assert_eq!(patch.len(), 2);
        assert!(patch.iter().all(|c| c.children().is_empty()), "groups go without children");
        let mut copy = base.clone();
        assert!(apply_cue_patch(&mut copy, &patch));
        assert_eq!(copy, edited);
        assert_eq!(cue_patch(&edited, &edited).unwrap().len(), 0);

        let mut moved = base.clone();
        moved.cue_list.cues.swap(0, 1);
        assert!(cue_patch(&base, &moved).is_none(), "reordered");
        let mut added = base.clone();
        added.cue_list.cues.push(Cue::new(CueKind::Memo));
        assert!(cue_patch(&base, &added).is_none(), "added");
        let mut settings = base.clone();
        settings.settings.osc_port += 1;
        assert!(cue_patch(&base, &settings).is_none(), "settings");
        // A copy without the cue can't take the patch.
        let mut other = Workspace::default();
        assert!(!apply_cue_patch(&mut other, &patch));
    }

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
