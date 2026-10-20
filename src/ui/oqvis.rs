//! Open in OQVis (View menu, patch header): saves the patch as an MVR file in `<config>/oqvis/rig.mvr` and
//! starts OQVis (the 3D visualiser, a separate program) with it. Asked again while OQVis runs, only the file is
//! rewritten: OQVis reloads it. OQVis gets the show's DMX over Art-Net, sACN or its own link to OpenQ.
//!
//! The program is `OPENQ_OQVIS_EXE`, else the one picked here before, else `oqvis` next to this executable
//! (where OQup installs it with the lighting feature), else `oqvis` on the PATH; when none starts, a file dialog
//! asks for it. The pick is kept per computer in `<config>/oqvis.json`.

use super::App;
use crate::i18n::t;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

/// Largest `oqvis.json` read.
const MAX_FILE: u64 = 64 << 10;

/// What is kept per computer.
#[derive(Serialize, Deserialize, Default, Clone, PartialEq, Debug)]
#[serde(default)]
struct Saved {
    /// The OQVis program picked by hand.
    program: Option<PathBuf>,
}

impl Saved {
    fn path() -> Option<PathBuf> {
        crate::remote::config_dir().map(|d| d.join("oqvis.json"))
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

/// The OQVis started from here.
#[derive(Default)]
pub(super) struct OQVis {
    child: Option<Child>,
}

impl OQVis {
    /// OQVis started from here is still running.
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
    if let Some(p) = std::env::var_os("OPENQ_OQVIS_EXE") {
        return vec![p.into()];
    }
    let mut list: Vec<PathBuf> = saved.program.iter().cloned().collect();
    if let Some(dir) = std::env::current_exe().ok().as_deref().and_then(Path::parent) {
        list.push(dir.join(format!("oqvis{}", std::env::consts::EXE_SUFFIX)));
    }
    list.push(PathBuf::from("oqvis"));
    list
}

/// Starts `program` on `rig`.
fn start(program: &Path, rig: &Path) -> std::io::Result<Child> {
    Command::new(program).arg(rig).stdin(Stdio::null()).spawn()
}

impl App {
    /// Save the patch for OQVis and start it (or let the running one reload); outside the show lock: a file
    /// dialog may ask where OQVis is.
    pub(super) fn open_oqvis(&mut self) {
        let (design, scope) = {
            let show = self.show.lock();
            let scope = show.path.as_ref().and_then(|p| p.file_stem()).map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "OpenQ".into());
            (show.ws.lighting.clone(), scope)
        };
        if design.fixtures.is_empty() {
            return self.notify(t("Patch some fixtures first: OQVis shows the patch"));
        }
        let Some(dir) = crate::remote::config_dir().map(|d| d.join("oqvis")) else {
            return self.notify(t("No config folder: can't save the rig for OQVis"));
        };
        let rig = dir.join("rig.mvr");
        let written = crate::remote::create_config_dir(&dir)
            .map_err(anyhow::Error::from)
            .and_then(|()| crate::light::mvr::export(&design, &scope))
            .and_then(|b| crate::recovery::write_atomic(&rig, &b));
        if let Err(e) = written {
            return self.notify_error(tr!("Can't export {kind}: {e}", kind = "MVR", e = format!("{e:#}")));
        }
        if self.oqvis.running() {
            return self.notify(t("Rig sent to OQVis"));
        }
        let mut saved = Saved::load();
        let mut started = None;
        for program in candidates(&saved) {
            match start(&program, &rig) {
                Ok(child) => {
                    started = Some(child);
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return self.notify_error(tr!("Can't start OQVis: {e}", e)),
            }
        }
        if started.is_none() {
            let Some(program) = rfd::FileDialog::new().set_title(t("Where is OQVis?")).pick_file() else {
                return self.notify(t("OQVis not found: install it or pick the program"));
            };
            match start(&program, &rig) {
                Ok(child) => started = Some(child),
                Err(e) => return self.notify_error(tr!("Can't start OQVis: {e}", e)),
            }
            saved.program = Some(program);
            if let Err(e) = saved.save() {
                log::warn!("cannot save OQVis settings: {e:#}");
            }
        }
        self.oqvis.child = started;
        self.notify(t("OQVis started with the rig"));
    }
}
