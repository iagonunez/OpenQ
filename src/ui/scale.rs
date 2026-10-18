//! UI scale (Settings → General, Ctrl + / Ctrl −): egui's zoom factor on top of the display's own
//! scaling, so small or low-resolution displays can fit more and large ones read better. Per computer,
//! like the language: `<config>/ui-scale`.

use super::App;
use crate::i18n::t;

/// The scales offered (Ctrl + / Ctrl − step through them).
pub(super) const STEPS: [f32; 10] = [0.67, 0.75, 0.8, 0.9, 1.0, 1.1, 1.25, 1.5, 1.75, 2.0];

/// A saved scale outside this range is ignored.
const RANGE: std::ops::RangeInclusive<f32> = 0.5..=2.5;

fn config_file() -> Option<std::path::PathBuf> {
    crate::remote::config_dir().map(|d| d.join("ui-scale"))
}

/// The saved scale (1.0 when none was saved or the file is unreadable).
pub(super) fn load() -> f32 {
    config_file()
        .and_then(|p| crate::recovery::read_text(&p, 64).ok())
        .and_then(|s| s.trim().parse::<f32>().ok())
        .filter(|z| RANGE.contains(z))
        .unwrap_or(1.0)
}

fn save(scale: f32) -> anyhow::Result<()> {
    let path = config_file().ok_or_else(|| anyhow::anyhow!("no config folder"))?;
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(path, format!("{scale}"))?;
    Ok(())
}

/// "125 %".
fn label(scale: f32) -> String {
    format!("{:.0} %", scale * 100.0)
}

impl App {
    /// Applies the saved scale on the first frame. Screenshots (`OPENQ_SCREENSHOT`) keep 100 % (or
    /// their own `OPENQ_SCREENSHOT_ZOOM`) and never save.
    pub(super) fn sync_ui_scale(&mut self, ctx: &egui::Context) {
        if self.scale_applied || std::env::var_os("OPENQ_SCREENSHOT").is_some() {
            return;
        }
        self.scale_applied = true;
        ctx.set_zoom_factor(self.ui_scale);
    }

    /// Switches to `scale` now and saves it.
    pub(super) fn set_ui_scale(&mut self, ctx: &egui::Context, scale: f32) {
        let scale = scale.clamp(*RANGE.start(), *RANGE.end());
        if (scale - self.ui_scale).abs() < 0.001 {
            return;
        }
        self.ui_scale = scale;
        ctx.set_zoom_factor(scale);
        if std::env::var_os("OPENQ_SCREENSHOT").is_none()
            && let Err(e) = save(scale)
        {
            self.notify_error(tr!("Cannot save the interface scale: {e}", e));
        }
        self.notify(tr!("Interface scale {scale}", scale = label(scale)));
    }

    /// Ctrl + / Ctrl −: the next larger / smaller of [`STEPS`].
    pub(super) fn step_ui_scale(&mut self, ctx: &egui::Context, up: bool) {
        let now = self.ui_scale;
        let next = if up { STEPS.iter().copied().find(|s| *s > now + 0.001) } else { STEPS.iter().rev().copied().find(|s| *s < now - 0.001) };
        if let Some(s) = next {
            self.set_ui_scale(ctx, s);
        }
    }

    /// Settings → General: the scale drop-down, applied and saved at once.
    pub(super) fn ui_scale_setting(&mut self, ui: &mut egui::Ui) {
        let mut pick = self.ui_scale;
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt("ui_scale").selected_text(label(pick)).show_ui(ui, |ui| {
                for s in STEPS {
                    ui.selectable_value(&mut pick, s, label(s));
                }
            });
            ui.label(egui::RichText::new(t("Ctrl + / Ctrl − change it too")).weak());
        });
        if pick != self.ui_scale {
            self.set_ui_scale(ui.ctx(), pick);
        }
    }
}
