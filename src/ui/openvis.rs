//! View → Open in OpenVis: saves the patch as an MVR file in `<config>/openvis/rig.mvr` and starts OpenVis (the
//! 3D visualiser, a separate program) with it. While sharing is on, OpenVis also gets a login for this OpenQ
//! (a trusted device on loopback, Control role) through its environment, so it follows the show's DMX and
//! fixture selection without pairing. Asked again while OpenVis runs, only the file is rewritten: OpenVis
//! reloads it.
//!
//! The program is `OPENQ_OPENVIS_EXE`, else the one picked here before, else `openvis` next to this executable,
//! else `openvis` on the PATH; when none starts, a file dialog asks for it. The pick and the device id are kept
//! per computer in `<config>/openvis.json`.

use super::App;
use crate::i18n::t;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use uuid::Uuid;

/// Name of the trusted device OpenVis logs in as.
const DEVICE_NAME: &str = "OpenVis (this computer)";

/// Largest `openvis.json` read.
const MAX_FILE: u64 = 64 << 10;

/// What is kept per computer.
#[derive(Serialize, Deserialize, Default, Clone, PartialEq, Debug)]
#[serde(default)]
struct Saved {
    /// The OpenVis program picked by hand.
    program: Option<PathBuf>,
    /// The trusted device OpenVis logs in as.
    device: Option<Uuid>,
}

impl Saved {
    fn path() -> Option<PathBuf> {
        crate::remote::config_dir().map(|d| d.join("openvis.json"))
    }

    fn load() -> Self {
        Self::path().and_then(|p| crate::recovery::read_file(&p, MAX_FILE).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    fn save(&self) -> anyhow::Result<()> {
        let path = Self::path().ok_or_else(|| anyhow::anyhow!("no config folder"))?;
        if let Some(d) = path.parent() {
            crate::remote::create_config_dir(d)?;
        }
        crate::recovery::write_atomic(&path, &serde_json::to_vec_pretty(self)?)
    }
}

/// The OpenVis started from here.
#[derive(Default)]
pub(super) struct OpenVis {
    child: Option<Child>,
}

impl OpenVis {
    /// OpenVis started from here is still running.
    fn running(&mut self) -> bool {
        let alive = self.child.as_mut().is_some_and(|c| matches!(c.try_wait(), Ok(None)));
        if !alive {
            self.child = None;
        }
        alive
    }
}

/// Programs to try, in order.
fn candidates(saved: &Saved) -> Vec<PathBuf> {
    if let Some(p) = std::env::var_os("OPENQ_OPENVIS_EXE") {
        return vec![p.into()];
    }
    let mut list: Vec<PathBuf> = saved.program.iter().cloned().collect();
    if let Some(dir) = std::env::current_exe().ok().as_deref().and_then(Path::parent) {
        list.push(dir.join(format!("openvis{}", std::env::consts::EXE_SUFFIX)));
    }
    list.push(PathBuf::from("openvis"));
    list
}

/// Starts `program` on `rig`, with the login for this OpenQ if there is one.
fn start(program: &Path, rig: &Path, login: Option<&(String, Uuid, String)>) -> std::io::Result<Child> {
    let mut cmd = Command::new(program);
    cmd.arg(rig).stdin(Stdio::null());
    if let Some((address, device, token)) = login {
        cmd.env("OPENVIS_OPENQ_ADDRESS", address).env("OPENVIS_OPENQ_DEVICE", device.to_string()).env("OPENVIS_OPENQ_TOKEN", token);
    }
    cmd.spawn()
}

impl App {
    /// Save the patch for OpenVis and start it (or let the running one reload); outside the show lock: a file
    /// dialog may ask where OpenVis is.
    pub(super) fn open_openvis(&mut self) {
        let (design, scope) = {
            let show = self.show.lock();
            let scope = show.path.as_ref().and_then(|p| p.file_stem()).map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "OpenQ".into());
            (show.ws.lighting.clone(), scope)
        };
        if design.fixtures.is_empty() {
            return self.notify(t("Patch some fixtures first: OpenVis shows the patch"));
        }
        let Some(dir) = crate::remote::config_dir().map(|d| d.join("openvis")) else {
            return self.notify(t("No config folder: can't save the rig for OpenVis"));
        };
        let rig = dir.join("rig.mvr");
        let written = crate::remote::create_config_dir(&dir)
            .map_err(anyhow::Error::from)
            .and_then(|()| crate::light::mvr::export(&design, &scope))
            .and_then(|b| crate::recovery::write_atomic(&rig, &b));
        if let Err(e) = written {
            return self.notify_error(tr!("Can't export {kind}: {e}", kind = "MVR", e = format!("{e:#}")));
        }
        if self.openvis.running() {
            return self.notify(t("Rig sent to OpenVis"));
        }
        let before = Saved::load();
        let mut saved = before.clone();
        let login = self.remote.local_login(saved.device, DEVICE_NAME);
        let mut started = None;
        for program in candidates(&saved) {
            match start(&program, &rig, login.as_ref()) {
                Ok(child) => {
                    started = Some(child);
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return self.notify_error(tr!("Can't start OpenVis: {e}", e)),
            }
        }
        if started.is_none() {
            let Some(program) = rfd::FileDialog::new().set_title(t("Where is OpenVis?")).pick_file() else {
                return self.notify(t("OpenVis not found: install it or pick the program"));
            };
            match start(&program, &rig, login.as_ref()) {
                Ok(child) => {
                    saved.program = Some(program);
                    started = Some(child);
                }
                Err(e) => return self.notify_error(tr!("Can't start OpenVis: {e}", e)),
            }
        }
        self.openvis.child = started;
        if let Some(l) = &login {
            saved.device = Some(l.1);
        }
        if saved != before
            && let Err(e) = saved.save()
        {
            log::warn!("cannot save OpenVis settings: {e:#}");
        }
        self.notify(if login.is_some() {
            t("OpenVis started: it follows this show")
        } else {
            t("OpenVis started with the rig. Turn on sharing (Remote → Share this OpenQ) so it follows the show live")
        });
    }
}
