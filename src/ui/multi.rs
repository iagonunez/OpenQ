//! Edit → Copy cue properties / Paste cue properties…: pick what to copy from one cue onto the
//! selected ones ([`crate::multiedit::paste`]).

use super::{App, icons, theme};
use crate::i18n::t;
use crate::model::Cue;
use crate::multiedit::Prop;
use crate::show::Show;
use egui::RichText;

/// The copied cue and the paste window.
pub(super) struct PasteProps {
    /// Copied cue (without a group's children).
    pub(super) from: Option<Cue>,
    pub(super) open: bool,
    /// Ticked properties, by [`Prop::ALL`] position.
    chosen: [bool; Prop::ALL.len()],
}

impl Default for PasteProps {
    fn default() -> Self {
        // Timing and type settings: what is usually wanted, never the file or target.
        let chosen = Prop::ALL.map(|p| matches!(p, Prop::PreWait | Prop::PostWait | Prop::Continue | Prop::TypeSettings));
        Self { from: None, open: false, chosen }
    }
}

impl App {
    /// Remember the selected cue's properties for pasting.
    pub(super) fn copy_props(&mut self, show: &Show) {
        let Some(c) = self.primary().and_then(|id| show.ws.cue_list.find(id)) else { return };
        let name = format!("{} {}", c.number, c.display_name()).trim().to_string();
        self.paste_props.from = Some(c.shallow_clone());
        self.notify(crate::tr!("Copied the properties of {cue}", cue = name));
    }

    pub(super) fn paste_props_window(&mut self, ctx: &egui::Context, show: &mut Show) {
        let st = &mut self.paste_props;
        if !st.open {
            return;
        }
        let Some(from) = st.from.clone() else {
            st.open = false;
            return;
        };
        let targets: Vec<_> = self.selection.iter().copied().filter(|id| *id != from.id).collect();
        let mut open = true;
        let mut paste = false;
        let st = &mut self.paste_props;
        egui::Window::new(theme::with_icon(icons::CONTENT_COPY, t("Paste cue properties")))
            .id(egui::Id::new("paste-props"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                let label = format!("{} {}", from.number, from.display_name()).trim().to_string();
                ui.label(crate::tr!("From {cue} onto {n} selected cues:", cue = label, n = targets.len()));
                ui.add_space(4.0);
                for (p, on) in Prop::ALL.iter().zip(st.chosen.iter_mut()) {
                    ui.checkbox(on, p.label());
                }
                ui.add_space(4.0);
                ui.label(RichText::new(t("Type settings, media files and targets go only to cues of the same type.")).small().color(theme::TEXT_DIM));
                ui.separator();
                ui.horizontal(|ui| {
                    let any = st.chosen.iter().any(|c| *c);
                    if ui.add_enabled(any && !targets.is_empty(), egui::Button::new(theme::with_icon(icons::CHECK_CIRCLE, t("Paste")))).clicked() {
                        paste = true;
                    }
                    if ui.button(t("Cancel")).clicked() {
                        st.open = false;
                    }
                });
            });
        st.open &= open && !paste;
        if !paste {
            return;
        }
        let props: Vec<Prop> = Prop::ALL.into_iter().zip(st.chosen).filter(|(_, on)| *on).map(|(p, _)| p).collect();
        let mut changed = Vec::new();
        for id in targets {
            if let Some(c) = show.ws.cue_list.find_mut(id)
                && crate::multiedit::paste(&from, c, &props)
            {
                changed.push(id);
                if show.engine.instances.contains_key(&id) {
                    let c = c.shallow_clone();
                    show.media.update_live(&c);
                }
            }
        }
        let n = changed.len();
        if n > 0 {
            show.edited_cues(changed);
        }
        self.notify(crate::tr!("Pasted onto {n} cues", n));
    }
}
