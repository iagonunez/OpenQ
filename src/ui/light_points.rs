//! Focus points in the programmer: the points pool (aim the selection at a point; edit, move or
//! delete points) and calibration from fixtures aimed by hand: move a point to where the beams
//! really meet, or correct where a group of fixtures (a truss) really hangs ([`crate::light::focus`]).
//!
//! A fixture aimed at a point from the pool is remembered (`LightUi::aims`) while its pan or tilt
//! stays in the programmer, so the operator can nudge it onto the real place and then use it.

use super::lighting::{CardLook, card_grid, pool_card};
use super::{App, icons, theme};
use crate::i18n::t;
use crate::light::focus::{self, Aim, Axes, FocusError};
use crate::light::model::{from_stage, split_id, to_stage, Attribute, FixtureId, FocusPoint, GroupId, Lighting, PointId, Value};
use egui::{DragValue, RichText, Ui};
use std::collections::BTreeSet;

/// What the user is told when a calibration can't be worked out.
fn error_text(e: FocusError) -> &'static str {
    match e {
        FocusError::NoAims => t("Aim fixtures at the point first (click the point, then move them onto the real place)."),
        FocusError::NoReach => t("A beam points away from the place: check the fixture's position and its pan and tilt."),
        FocusError::Unclear => t("These beams can't tell the axes apart: aim another fixture of the group (away from the centre line) or at another point, or correct fewer axes."),
    }
}

/// Fixture numbers (not cells) of units.
fn fixtures_of(units: impl IntoIterator<Item = FixtureId>) -> Vec<FixtureId> {
    let set: BTreeSet<FixtureId> = units.into_iter().map(|u| split_id(u).0).collect();
    set.into_iter().collect()
}

impl App {
    /// Aim the selected units that have pan or tilt at `point` (as references, so they follow
    /// the point), and remember the aim for calibration. Returns how many were aimed.
    pub(super) fn apply_point(&mut self, l: &Lighting, point: PointId) -> usize {
        let mut n = 0;
        for f in &self.light.selection {
            let attrs = l.unit_attributes(*f);
            let aims: Vec<Attribute> = [Attribute::Pan, Attribute::Tilt].into_iter().filter(|a| attrs.contains(a)).collect();
            if aims.is_empty() {
                continue;
            }
            let v = self.light.programmer.entry(*f).or_default();
            for a in aims {
                v.insert(a, Value::Point { point });
            }
            self.light.aims.insert(*f, point);
            n += 1;
        }
        n
    }

    /// Forget aims whose pan and tilt left the programmer (cleared, or recorded and cleared).
    pub(super) fn prune_aims(&mut self) {
        let programmer = &self.light.programmer;
        self.light.aims.retain(|f, _| programmer.get(f).is_some_and(|v| v.contains_key(&Attribute::Pan) || v.contains_key(&Attribute::Tilt)));
    }

    /// The beam of unit `f` as the programmer aims it now, towards `target`.
    fn hand_aim(&self, l: &Lighting, f: FixtureId, target: [f32; 3]) -> Option<Aim> {
        let (fixture, _) = l.unit(f)?;
        let physical = &l.fixture_type(&fixture.fixture_type)?.physical;
        let v = self.light.programmer.get(&f)?;
        let get = |a: Attribute| v.get(&a).and_then(|x| l.resolve(f, a, *x));
        let (pan, tilt) = (get(Attribute::Pan)?, get(Attribute::Tilt)?);
        Aim::new(fixture, physical, pan, tilt, target)
    }

    /// Remembered aims (at `point`, or at any point): unit and beam.
    fn hand_aims(&self, l: &Lighting, point: Option<PointId>) -> Vec<(FixtureId, Aim)> {
        self.light
            .aims
            .iter()
            .filter(|(_, p)| point.is_none_or(|x| x == **p))
            .filter_map(|(f, p)| Some((*f, self.hand_aim(l, *f, l.point(*p)?.position)?)))
            .collect()
    }

    /// The units in `aims` point at their points again (references): after a correction the
    /// patch agrees with what they were aimed at by hand.
    fn relink(&mut self, units: &[FixtureId]) {
        for f in units {
            if let Some(point) = self.light.aims.get(f).copied()
                && let Some(v) = self.light.programmer.get_mut(f)
            {
                for a in [Attribute::Pan, Attribute::Tilt] {
                    if let Some(x) = v.get_mut(&a) {
                        *x = Value::Point { point };
                    }
                }
            }
        }
    }

    /// Focus points as cards: click aims the selection at one, right-click edits, moves or
    /// deletes it; + makes one where the selected fixtures point. Then the rig correction, while
    /// fixtures are aimed at points.
    pub(super) fn point_pool(&mut self, ui: &mut Ui, l: &mut Lighting, max_h: f32) {
        ui.horizontal(|ui| {
            ui.label(theme::caption(t("Focus points")));
            ui.label(RichText::new(l.points.len().to_string()).font(theme::semibold(10.5)).color(theme::TEXT_DIM));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let full = l.points.len() >= crate::limits::MAX_POINTS;
                if ui
                    .add_enabled(!full, egui::Button::new(icons::ADD))
                    .on_hover_text(t(
                        "New focus point where the selected fixtures point (on the floor for one fixture, where the beams cross for more); right-click a point to edit, move or delete it",
                    ))
                    .clicked()
                {
                    self.new_point(l);
                }
            });
        });
        ui.add_space(4.0);
        enum Act {
            Apply(PointId),
            Locate(PointId),
            Delete(PointId),
        }
        let mut action = None;
        let used: BTreeSet<PointId> = self
            .light
            .selection
            .iter()
            .filter_map(|f| self.light.programmer.get(f))
            .flat_map(|v| v.values())
            .filter_map(|v| match v {
                Value::Point { point } => Some(*point),
                _ => None,
            })
            .collect();
        let aimed: Vec<(PointId, usize)> = l.points.iter().map(|p| (p.id, self.light.aims.values().filter(|x| **x == p.id).count())).collect();
        let m = crate::limits::MAX_DISTANCE;
        let mut points = std::mem::take(&mut l.points);
        egui::ScrollArea::vertical().id_salt("point_cards").max_height(max_h).auto_shrink([false, true]).show(ui, |ui| {
            let ids: Vec<usize> = (0..points.len()).collect();
            card_grid(ui, &ids, |ui, i, size| {
                let p = &mut points[*i];
                let name = if p.name.is_empty() { tr!("Point {n}", n = p.id.to_string()) } else { p.name.clone() };
                let r = pool_card(ui, size, icons::MY_LOCATION, p.id, &name, if used.contains(&p.id) { CardLook::On } else { CardLook::Off }, None);
                let [x, y, z] = to_stage(p.position);
                let r = r.on_hover_text(format!("{} {name}\nx {x:.2} · y {y:.2} · z {z:.2} m", p.id));
                if r.clicked() {
                    action = Some(Act::Apply(p.id));
                }
                r.context_menu(|ui| {
                    ui.horizontal(|ui| {
                        ui.label(t("Name"));
                        ui.add(egui::TextEdit::singleline(&mut p.name).desired_width(140.0));
                    });
                    ui.horizontal(|ui| {
                        let mut v = to_stage(p.position);
                        let mut changed = false;
                        for (label, x) in ["x", "y", "z"].into_iter().zip(v.iter_mut()) {
                            ui.label(label);
                            changed |= ui.add(DragValue::new(x).speed(0.05).range(-m..=m).max_decimals(2).suffix(" m")).changed();
                        }
                        if changed {
                            p.position = from_stage(v);
                        }
                    });
                    ui.label(RichText::new(t("x across, y depth (+ upstage), z height")).weak().small());
                    ui.separator();
                    let n = aimed.iter().find(|a| a.0 == p.id).map_or(0, |a| a.1);
                    if ui
                        .add_enabled(n > 0, egui::Button::new(theme::with_icon(icons::MY_LOCATION, &tr!("Move it where the aimed fixtures point ({n})", n = n.to_string()))))
                        .on_hover_text(t("Uses the fixtures aimed at this point and moved onto the real place: one beam meets the point's height, several cross"))
                        .clicked()
                    {
                        action = Some(Act::Locate(p.id));
                        ui.close();
                    }
                    if ui.button(theme::with_icon(icons::DELETE, t("Delete point"))).clicked() {
                        action = Some(Act::Delete(p.id));
                        ui.close();
                    }
                });
            });
            if points.is_empty() {
                ui.label(RichText::new(t("Aim fixtures at a place, then make a focus point there with +. Cues aimed at a point follow it when it or the fixtures move.")).color(theme::TEXT_DIM));
            }
        });
        l.points = points;
        match action {
            Some(Act::Apply(id)) => {
                let snapshot = l.clone();
                self.apply_point(&snapshot, id);
            }
            Some(Act::Locate(id)) => self.locate_point(l, id),
            Some(Act::Delete(id)) => {
                crate::light::model::unlink_point(l, id, &mut self.light.programmer);
                self.light.aims.retain(|_, p| *p != id);
                l.delete_point(id);
            }
            None => {}
        }
        if !self.light.aims.is_empty() {
            ui.add_space(6.0);
            self.rig_correction(ui, l);
        }
    }

    /// A new point where the selected fixtures with pan/tilt in the programmer point (the floor
    /// for one, the crossing for more; the origin when none is aimed). They are aimed at it.
    fn new_point(&mut self, l: &mut Lighting) {
        let aims: Vec<Aim> = self.light.selection.iter().filter_map(|f| self.hand_aim(l, *f, [0.0; 3])).collect();
        let position = match focus::locate(&aims, 0.0) {
            Ok(fit) => fit.value,
            Err(FocusError::NoAims) => [0.0; 3],
            Err(e) => {
                self.notify(error_text(e));
                return;
            }
        };
        let id = l.points.iter().map(|p| p.id).max().unwrap_or(0) + 1;
        l.points.push(FocusPoint { id, name: String::new(), position: position.map(|v| (v * 100.0).round() / 100.0) });
        let snapshot = l.clone();
        self.apply_point(&snapshot, id);
    }

    /// Move point `id` where the fixtures aimed at it point.
    fn locate_point(&mut self, l: &mut Lighting, id: PointId) {
        let aims = self.hand_aims(l, Some(id));
        let Some(p) = l.point(id) else { return };
        let beams: Vec<Aim> = aims.iter().map(|a| a.1).collect();
        match focus::locate(&beams, p.position[1]) {
            Ok(fit) => {
                if let Some(p) = l.points.iter_mut().find(|p| p.id == id) {
                    p.position = fit.value;
                }
                let units: Vec<FixtureId> = aims.iter().map(|a| a.0).collect();
                self.relink(&units);
                let [x, y, z] = to_stage(fit.value).map(|v| format!("{v:.2}"));
                let msg = if beams.len() > 1 {
                    tr!("Point moved to x {x} · y {y} · z {z} m (beams within {cm} cm).", x = x, y = y, z = z, cm = format!("{:.0}", fit.miss * 100.0))
                } else {
                    tr!("Point moved to x {x} · y {y} · z {z} m.", x = x, y = y, z = z)
                };
                self.notify(msg);
            }
            Err(e) => self.notify(error_text(e)),
        }
    }

    /// Correct where a group of fixtures hangs from the aims of its fixtures: the group (or the
    /// aimed fixtures) moves along the chosen axes, with the plot bar it hangs on.
    fn rig_correction(&mut self, ui: &mut Ui, l: &mut Lighting) {
        let aims = self.hand_aims(l, None);
        let aimed = fixtures_of(aims.iter().map(|a| a.0));
        ui.horizontal(|ui| {
            ui.label(theme::caption(t("Correct the rig")));
            let list: Vec<String> = self
                .light
                .aims
                .iter()
                .map(|(f, p)| format!("{} → {}", crate::light::model::fixture_label(*f), l.point(*p).map_or_else(|| p.to_string(), |p| if p.name.is_empty() { p.id.to_string() } else { p.name.clone() })))
                .collect();
            ui.label(RichText::new(tr!("{n} aimed", n = self.light.aims.len().to_string())).color(theme::TEXT_DIM)).on_hover_text(list.join("\n"));
        });
        // Groups holding an aimed fixture.
        let groups: Vec<(GroupId, String)> = l
            .groups
            .iter()
            .filter(|g| fixtures_of(g.fixtures.iter().copied()).iter().any(|f| aimed.contains(f)))
            .map(|g| (g.id, if g.name.is_empty() { tr!("Group {n}", n = g.id.to_string()) } else { format!("{} {}", g.id, g.name) }))
            .collect();
        if self.light.fix_group.is_some_and(|g| !groups.iter().any(|x| x.0 == g)) {
            self.light.fix_group = None;
        }
        let shown = |g: Option<GroupId>| g.and_then(|g| groups.iter().find(|x| x.0 == g)).map_or_else(|| t("Aimed fixtures").to_string(), |x| x.1.clone());
        theme::wrapped_row(ui, |ui| {
            theme::keep_together(ui, "fix_group", |ui| {
                ui.label(t("Move"));
                egui::ComboBox::from_id_salt("fix_group").selected_text(shown(self.light.fix_group)).show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.light.fix_group, None, t("Aimed fixtures"));
                    for (id, name) in &groups {
                        ui.selectable_value(&mut self.light.fix_group, Some(*id), name);
                    }
                });
            });
            theme::keep_together(ui, "fix_axes", |ui| {
                ui.label(t("along"));
                egui::ComboBox::from_id_salt("fix_axes").selected_text(self.light.fix_axes.label()).show_ui(ui, |ui| {
                    for a in Axes::ALL {
                        ui.selectable_value(&mut self.light.fix_axes, a, a.label());
                    }
                });
            });
        });
        theme::wrapped_row(ui, |ui| {
            if ui
                .button(theme::with_icon(icons::OPEN_WITH, t("Correct")))
                .on_hover_text(t("Move the fixtures so the beams aimed by hand hit their points from the patch; every cue aimed at points follows"))
                .clicked()
            {
                let units: Vec<FixtureId> = match self.light.fix_group.and_then(|g| l.groups.iter().find(|x| x.id == g)) {
                    Some(g) => g.fixtures.clone(),
                    None => aimed.clone(),
                };
                let moving = fixtures_of(units);
                let used: Vec<(FixtureId, Aim)> = aims.iter().filter(|a| moving.contains(&split_id(a.0).0)).copied().collect();
                let beams: Vec<Aim> = used.iter().map(|a| a.1).collect();
                match focus::rig_offset(&beams, self.light.fix_axes) {
                    Ok(fit) => {
                        focus::shift(l, &moving, fit.value);
                        let units: Vec<FixtureId> = used.iter().map(|a| a.0).collect();
                        self.relink(&units);
                        let [x, y, z] = to_stage(fit.value).map(|v| format!("{v:+.2}"));
                        let n = moving.len().to_string();
                        let msg = if beams.len() > 1 || self.light.fix_axes == Axes::Height {
                            tr!("Moved {n} fixtures by x {x} · y {y} · z {z} m (beams within {cm} cm).", n = n, x = x, y = y, z = z, cm = format!("{:.0}", fit.miss * 100.0))
                        } else {
                            tr!("Moved {n} fixtures by x {x} · y {y} · z {z} m.", n = n, x = x, y = y, z = z)
                        };
                        self.notify(msg);
                    }
                    Err(e) => self.notify(error_text(e)),
                }
            }
            if ui.button(theme::with_icon(icons::CLOSE, t("Forget aims"))).on_hover_text(t("The aimed fixtures stay in the programmer")).clicked() {
                self.light.aims.clear();
            }
        });
    }
}
