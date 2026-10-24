//! Lighting design files other programs read and write: MVR (the patch with positions and plot
//! shapes, from visualisers and CAD) and USITT ASCII (patch, cues, groups and submasters, from
//! conventional desks).
//!
//! Files are read on a thread of their own (an MVR can hold many GDTFs) and put into the design
//! by [`App::poll_design_imports`] in a later frame; nothing already in the design changes
//! ([`crate::light::mvr::merge`], [`crate::light::usitt::merge`]).

use super::App;
use crate::i18n::t;
use crate::light::model::FixtureType;
use crate::light::{mvr, usitt};
use parking_lot::Mutex;
use std::path::PathBuf;
use std::sync::Arc;

/// A file read in the background, waiting to go into the design.
pub(crate) enum DesignImport {
    /// An MVR file.
    Mvr(PathBuf, Result<mvr::Imported, String>),
    /// A USITT ASCII file.
    Usitt(PathBuf, Result<usitt::File, String>),
}

/// Files being read, shared with their threads.
pub(crate) type Imports = Arc<Mutex<Vec<DesignImport>>>;

fn file_name(p: &std::path::Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

impl App {
    /// Read `path` with `parse` on a thread of its own; the result goes to `poll_design_imports`.
    fn read_in_background(&mut self, ctx: &egui::Context, path: PathBuf, parse: impl FnOnce(PathBuf) -> DesignImport + Send + 'static) {
        let done = self.light.design_imports.clone();
        let ctx = ctx.clone();
        let job = move || {
            let r = parse(path);
            done.lock().push(r);
            ctx.request_repaint();
        };
        if let Err(e) = std::thread::Builder::new().name("opnq-design-import".into()).spawn(job) {
            self.notify_error(tr!("Can't import the file: {e}", e = e.to_string()));
        }
    }

    /// Pick an MVR file and add its fixtures and plot shapes to the patch (outside the show
    /// lock: the dialog blocks).
    pub(super) fn import_mvr(&mut self, ctx: &egui::Context) {
        if !self.can_design() {
            return;
        }
        let Some(path) = rfd::FileDialog::new().add_filter("MVR", &[mvr::EXTENSION]).pick_file() else { return };
        // Types to match by name: the show's, the library's, the built-ins.
        let mut known: Vec<FixtureType> = self.show.lock().ws.lighting.fixture_types.clone();
        known.extend(self.light.types.refresh().items.iter().cloned());
        known.extend(crate::light::library::generics());
        self.read_in_background(ctx, path, move |path| {
            let r = crate::recovery::catch(|| {
                let bytes = crate::recovery::read_file(&path, mvr::MAX_FILE)?;
                let known: Vec<&FixtureType> = known.iter().collect();
                mvr::import(&bytes, &known)
            });
            DesignImport::Mvr(path, r.map_err(|e| format!("{e:#}")))
        });
    }

    /// Pick a USITT ASCII file and add its cues, groups, submasters and patch to the design.
    pub(super) fn import_usitt(&mut self, ctx: &egui::Context) {
        if !self.can_design() {
            return;
        }
        let Some(path) = rfd::FileDialog::new().add_filter(t("USITT ASCII"), &usitt::EXTENSIONS).pick_file() else { return };
        self.read_in_background(ctx, path, |path| {
            let r = crate::recovery::catch(|| {
                let bytes = crate::recovery::read_file(&path, usitt::MAX_FILE)?;
                // Old desks wrote Latin-1: keep what is ASCII, replace the rest.
                usitt::read(&String::from_utf8_lossy(&bytes))
            });
            DesignImport::Usitt(path, r.map_err(|e| format!("{e:#}")))
        });
    }

    /// Save the design as a USITT ASCII file (outside the show lock: the dialog blocks).
    pub(super) fn export_usitt(&mut self) {
        let (design, scope) = {
            let show = self.show.lock();
            let scope = show.path.as_ref().and_then(|p| p.file_stem()).map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "OpnQ".into());
            (show.ws.lighting.clone(), scope)
        };
        let Some(path) = rfd::FileDialog::new().add_filter(t("USITT ASCII"), &usitt::EXTENSIONS[..1]).set_file_name(format!("{scope}.asc")).save_file() else { return };
        let (text, notes) = usitt::write(&design, &scope);
        for n in &notes {
            log::warn!("USITT ASCII export ({}): {n}", path.display());
        }
        match crate::recovery::write_atomic(&path, text.as_bytes()) {
            Ok(()) if notes.is_empty() => self.notify(tr!("Saved {file}", file = path.display().to_string())),
            Ok(()) => self.notify(tr!("Saved {file} ({n} notes in the error log)", file = path.display().to_string(), n = notes.len().to_string())),
            Err(e) => self.notify_error(tr!("Can't export {kind}: {e}", kind = "USITT ASCII", e = format!("{e:#}"))),
        }
    }

    /// Put files read in the background into the design (called every frame).
    pub(super) fn poll_design_imports(&mut self, show: &mut crate::show::Show) {
        let done = std::mem::take(&mut *self.light.design_imports.lock());
        for d in done {
            let (path, notes, what) = match d {
                DesignImport::Mvr(path, Err(e)) | DesignImport::Usitt(path, Err(e)) => {
                    self.notify_error(tr!("Can't import {file}: {e}", file = file_name(&path), e = e));
                    continue;
                }
                DesignImport::Mvr(path, Ok(imp)) => {
                    let mut l = show.ws.lighting.clone();
                    let n = imp.fixtures.len();
                    let notes = mvr::merge(&mut l, imp);
                    show.ws.lighting = l;
                    (path, notes, tr!("{n} fixtures", n = n.to_string()))
                }
                DesignImport::Usitt(path, Ok(file)) => {
                    let mut l = show.ws.lighting.clone();
                    let name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                    let notes = usitt::merge(&mut l, &file, &name);
                    let mut ws = crate::model::Workspace { lighting: l, ..Default::default() };
                    if let Err(e) = crate::limits::check_counts(&ws) {
                        self.notify_error(tr!("Can't import {file}: {e}", file = file_name(&path), e = e));
                        continue;
                    }
                    show.ws.lighting = std::mem::take(&mut ws.lighting);
                    (path, notes, tr!("{n} cues", n = file.cues.len().to_string()))
                }
            };
            crate::limits::lighting(&mut show.ws.lighting);
            show.edited_outside_cues();
            for n in &notes {
                log::warn!("import ({}): {n}", path.display());
            }
            let file = file_name(&path);
            self.notify(if notes.is_empty() {
                tr!("Imported {file}: {what}", file = file, what = what)
            } else {
                tr!("Imported {file}: {what} ({n} notes in the error log)", file = file, what = what, n = notes.len().to_string())
            });
        }
    }
}
