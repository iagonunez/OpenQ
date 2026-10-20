//! Focus points in the programmer: the points pool under the groups (a click aims the selection at a
//! point and opens its settings row: name, delete) and calibration from fixtures aimed by hand:
//! correct where a group of fixtures (a truss) really hangs ([`crate::light::focus`]). Points
//! themselves only move when the operator drags them on the Plan view (Patch page).
//!
//! A point is a reference: a fixture aimed at it from the pool (or `point N`) keeps the reference
//! while it is moved by hand, and what changes is where it aims measured from the point
//! ([`Value::Point`] `offset`). So a cue can aim fixtures at the point itself, or spread them
//! round it (an arc, a line), and still follow the point when it moves. Only fixtures the operator
//! aimed from a point refer to one; other pan/tilt moves stay plain levels.
//! The Position page's "Spread round the point" row lays the selected fixtures out round their
//! point as a line, an arc or a circle ([`focus::Spread`]) in one go.
//!
//! The same offsets serve calibration: a fixture aimed at a point and moved onto the real place
//! tells where its truss really hangs. The operator picks the group to correct (nothing is guessed
//! about which fixtures share a truss); correcting moves those fixtures in the patch and folds
//! their offsets back to zero, so every cue aimed at points lights the right places again.

use super::lighting::{CardLook, card_grid, pool_card};
use super::{App, icons, theme};
use crate::i18n::t;
use crate::light::focus::{self, Aim, Axes, FocusError, SpreadShape};
use crate::light::model::{from_stage, split_id, to_stage, Attribute, FixtureId, FocusPoint, GroupId, Lighting, PointId, Value};
use egui::{DragValue, RichText, Ui};
use crate::light::focus::reference;
use std::collections::{BTreeMap, BTreeSet};

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
    /// Units whose pan/tilt in the programmer refer to a focus point, with the point and offset.
    fn aims(&self) -> Vec<(FixtureId, PointId, [f32; 3])> {
        self.light.programmer.iter().filter_map(|(f, v)| reference(v).map(|(p, o)| (*f, p, o))).collect()
    }

    /// Aim the selected units that have pan or tilt at `point` itself (as references, so they
    /// follow the point; moving them by hand then measures from it). Returns how many were aimed.
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
                v.insert(a, Value::point(point));
            }
            n += 1;
        }
        n
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

    /// Units aimed from a point (`point`, or any point) as calibration beams: where the beam
    /// really goes (offset included) and the point it was meant to hit.
    fn hand_aims(&self, l: &Lighting, point: Option<PointId>) -> Vec<(FixtureId, Aim)> {
        self.aims()
            .into_iter()
            .filter(|(_, p, _)| point.is_none_or(|x| x == *p))
            .filter_map(|(f, p, _)| Some((f, self.hand_aim(l, f, l.point(p)?.position)?)))
            .collect()
    }

    /// The units aim at their points again (offsets back to zero): after a correction the patch
    /// agrees with what they were aimed at by hand.
    fn relink(&mut self, units: &[FixtureId]) {
        for f in units {
            if let Some(v) = self.light.programmer.get_mut(f) {
                for x in v.values_mut() {
                    if let Value::Point { offset, .. } = x {
                        *offset = [0.0; 3];
                    }
                }
            }
        }
    }

    /// Position page: where the selected units aimed from focus points aim, measured from the
    /// point (stage axes). Dragging moves them all by the same amount (an arc stays an arc);
    /// "At the point" aims them at the point itself, "Flip" reaches the same place with the head
    /// turned over, "Detach" keeps the aim as plain pan/tilt.
    pub(super) fn point_offsets(&mut self, ui: &mut Ui, l: &Lighting) {
        let refs: Vec<(FixtureId, PointId, [f32; 3])> =
            self.light.selection.iter().filter_map(|f| reference(self.light.programmer.get(f)?).map(|(p, o)| (*f, p, o))).collect();
        let Some(&(_, first, offset)) = refs.first() else { return };
        let mut points: Vec<PointId> = refs.iter().map(|r| r.1).collect();
        points.dedup();
        let name = |id: PointId| l.point(id).map_or_else(|| id.to_string(), |p| if p.name.is_empty() { tr!("Point {n}", n = id.to_string()) } else { p.name.clone() });
        let label = if points.len() == 1 { name(first) } else { tr!("{n} points", n = points.len().to_string()) };
        let spread = refs.iter().any(|r| r.2 != offset);
        let m = crate::limits::MAX_DISTANCE;
        theme::wrapped_row(ui, |ui| {
            theme::keep_together(ui, "point_from", |ui| {
                ui.label(RichText::new(icons::MY_LOCATION).font(theme::icon_font(14.0)).color(theme::ACCENT));
                ui.label(tr!("From {point}", point = label)).on_hover_text(t("Where these fixtures aim is measured from the point: move the point and the cue follows. Move them by hand to aim beside it."));
            });
            theme::keep_together(ui, "point_offset", |ui| {
                let shown = to_stage(offset);
                let mut v = shown;
                for (axis, x) in ["x ", "y ", "z "].into_iter().zip(v.iter_mut()) {
                    ui.add(DragValue::new(x).speed(0.02).range(-m..=m).max_decimals(2).prefix(axis).suffix(" m"))
                        .on_hover_text(if spread { t("Offset of the first fixture; a change moves every selected fixture by the same amount") } else { t("Offset from the point: x across, y depth (+ upstage), z height") });
                }
                if v != shown {
                    let d = from_stage([v[0] - shown[0], v[1] - shown[1], v[2] - shown[2]]);
                    for (f, _, _) in &refs {
                        if let Some(values) = self.light.programmer.get_mut(f) {
                            for x in values.values_mut() {
                                if let Value::Point { offset, .. } = x {
                                    *offset = [0, 1, 2].map(|i| (offset[i] + d[i]).clamp(-m, m));
                                }
                            }
                        }
                    }
                }
            });
            theme::keep_together(ui, "point_offset_buttons", |ui| {
                if ui.add_enabled(refs.iter().any(|r| r.2 != [0.0; 3]), egui::Button::new(t("At the point"))).on_hover_text(t("Aim the selected fixtures at the point itself")).clicked() {
                    let units: Vec<FixtureId> = refs.iter().map(|r| r.0).collect();
                    self.relink(&units);
                }
                if ui.button(t("Flip")).on_hover_text(t("Reach the same place the other way: pan half a turn round, tilt the other side")).clicked() {
                    for (f, _, _) in &refs {
                        if let Some(values) = self.light.programmer.get_mut(f) {
                            for x in values.values_mut() {
                                if let Value::Point { flip, .. } = x {
                                    *flip = !*flip;
                                }
                            }
                        }
                    }
                }
                if ui.button(t("Detach")).on_hover_text(t("Keep where they aim as plain pan and tilt, no longer measured from the point")).clicked() {
                    for (f, _, _) in &refs {
                        if let Some(values) = self.light.programmer.get_mut(f) {
                            let mut look = BTreeMap::from([(*f, std::mem::take(values))]);
                            crate::light::model::unlink(l, |v| v.point_id().is_some(), &mut look);
                            *values = look.remove(f).unwrap_or_default();
                        }
                    }
                }
            });
        });
        if refs.len() > 1 {
            self.point_spread_row(ui, &refs);
        }
        ui.add_space(4.0);
    }

    /// Position page: lay the selected units aimed from points out round their point as a line,
    /// an arc or a circle (offsets on the point's height plane, in selection order; units aimed
    /// at different points are spread round each one). Applied on every change of the settings
    /// and with the button; each unit keeps its height offset.
    fn point_spread_row(&mut self, ui: &mut Ui, refs: &[(FixtureId, PointId, [f32; 3])]) {
        let before = self.light.point_spread;
        let s = &mut self.light.point_spread;
        let mut apply = false;
        theme::wrapped_row(ui, |ui| {
            theme::keep_together(ui, "point_spread", |ui| {
                ui.label(t("Spread round the point"))
                    .on_hover_text(t("Aim the selected fixtures round their point, in the order they were selected: the cue keeps the form and follows the point"));
                egui::ComboBox::from_id_salt("point_spread_shape").selected_text(s.shape.label()).show_ui(ui, |ui| {
                    for shape in SpreadShape::ALL {
                        ui.selectable_value(&mut s.shape, shape, shape.label());
                    }
                });
            });
            theme::keep_together(ui, "point_spread_values", |ui| {
                let (size, tip) = if s.shape == SpreadShape::Line {
                    (t("Width"), t("From the first fixture to the last"))
                } else {
                    (t("Radius"), t("Distance from the point"))
                };
                ui.add(DragValue::new(&mut s.size).speed(0.02).range(0.0..=crate::limits::MAX_DISTANCE).max_decimals(2).prefix(format!("{size} ")).suffix(" m")).on_hover_text(tip);
                if s.shape == SpreadShape::Arc {
                    ui.add(DragValue::new(&mut s.span).speed(0.5).range(0.0..=360.0).max_decimals(1).prefix(format!("{} ", t("Angle"))).suffix("°"))
                        .on_hover_text(t("Degrees from the first fixture to the last"));
                }
                ui.add(DragValue::new(&mut s.turn).speed(0.5).range(-360.0..=360.0).max_decimals(1).prefix(format!("{} ", t("Rotation"))).suffix("°"))
                    .on_hover_text(t("Direction of the form seen from above: at 0 a line runs across the stage and an arc bulges downstage"));
                apply = ui.button(t("Spread out")).on_hover_text(t("Spread the selected fixtures round their point")).clicked();
            });
        });
        if !apply && self.light.point_spread == before {
            return;
        }
        // Units of each point, in selection order.
        let mut points: Vec<PointId> = refs.iter().map(|r| r.1).collect();
        points.sort_unstable();
        points.dedup();
        for point in points {
            let units: Vec<FixtureId> = refs.iter().filter(|r| r.1 == point).map(|r| r.0).collect();
            for (f, o) in units.iter().zip(self.light.point_spread.offsets(units.len())) {
                if let Some(values) = self.light.programmer.get_mut(f) {
                    for x in values.values_mut() {
                        if let Value::Point { offset, .. } = x {
                            *offset = [o[0], offset[1], o[2]];
                        }
                    }
                }
            }
        }
    }

    /// Focus points as cards: click aims the selection at one and shows its settings (name,
    /// delete) in a row under the cards; + makes one where the selected fixtures point. Then the
    /// rig correction, once a fixture aimed from a point was moved off it.
    pub(super) fn point_pool(&mut self, ui: &mut Ui, l: &mut Lighting, max_h: f32) {
        ui.horizontal(|ui| {
            ui.label(theme::caption(t("Focus points")));
            ui.label(RichText::new(l.points.len().to_string()).font(theme::semibold(10.5)).color(theme::TEXT_DIM));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let full = l.points.len() >= crate::limits::MAX_POINTS;
                if ui
                    .add_enabled(!full, egui::Button::new(icons::ADD))
                    .on_hover_text(t("New focus point where the selected fixtures point (on the floor for one fixture, where the beams cross for more)"))
                    .clicked()
                {
                    self.new_point(l);
                }
            });
        });
        ui.add_space(4.0);
        enum Act {
            Apply(PointId),
            Delete(PointId),
        }
        let mut action = None;
        let used: BTreeSet<PointId> = self
            .light
            .selection
            .iter()
            .filter_map(|f| self.light.programmer.get(f))
            .flat_map(|v| v.values())
            .filter_map(Value::point_id)
            .collect();
        let aims = self.aims();
        if self.light.point_sel.is_some_and(|id| l.point(id).is_none()) {
            self.light.point_sel = None;
        }
        let picked = self.light.point_sel;
        egui::ScrollArea::vertical().id_salt("point_cards").max_height(max_h).auto_shrink([false, true]).show(ui, |ui| {
            card_grid(ui, &l.points, |ui, p, size| {
                let name = if p.name.is_empty() { tr!("Point {n}", n = p.id.to_string()) } else { p.name.clone() };
                let look = if used.contains(&p.id) || picked == Some(p.id) { CardLook::On } else { CardLook::Off };
                let r = pool_card(ui, size, icons::MY_LOCATION, p.id, &name, look, None);
                let [x, y, z] = to_stage(p.position);
                let r = r.on_hover_text(format!("{} {name}\nx {x:.2} · y {y:.2} · z {z:.2} m\n{}", p.id, t("Click to aim the selection at it and edit it below")));
                if r.clicked() {
                    action = Some(Act::Apply(p.id));
                }
            });
            if l.points.is_empty() {
                ui.label(RichText::new(t("Aim fixtures at a place, then make a focus point there with +. Cues aimed at a point follow it when it or the fixtures move.")).color(theme::TEXT_DIM));
            }
        });
        // The picked point's settings, in a row under the cards. Its place is changed only by
        // dragging it on the Plan view, so cues never move because of a calibration.
        if let Some(id) = picked
            && let Some(p) = l.points.iter_mut().find(|p| p.id == id)
        {
            ui.add_space(4.0);
            theme::wrapped_row(ui, |ui| {
                ui.add(egui::TextEdit::singleline(&mut p.name).hint_text(tr!("Point {n}", n = id.to_string())).desired_width(110.0));
                let [x, y, z] = to_stage(p.position);
                ui.label(RichText::new(format!("x {x:.2} · y {y:.2} · z {z:.2} m")).color(theme::TEXT_DIM))
                    .on_hover_text(t("x across, y depth (+ upstage), z height. Drag the point on Patch → Plan to move it."));
                theme::keep_together(ui, "point_buttons", |ui| {
                    if ui.small_button(icons::DELETE).on_hover_text(t("Delete point")).clicked() {
                        action = Some(Act::Delete(id));
                    }
                    if ui.small_button(icons::CLOSE).on_hover_text(t("Close")).clicked() {
                        self.light.point_sel = None;
                    }
                });
            });
        }
        match action {
            Some(Act::Apply(id)) => {
                let snapshot = l.clone();
                self.apply_point(&snapshot, id);
                self.light.point_sel = Some(id);
            }
            Some(Act::Delete(id)) => {
                crate::light::model::unlink_point(l, id, &mut self.light.programmer);
                l.delete_point(id);
                self.light.point_sel = None;
            }
            None => {}
        }
        // Calibration once a fixture aimed from a point was moved off it.
        if aims.iter().any(|a| a.2 != [0.0; 3]) {
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
        self.light.point_sel = Some(id);
    }

    /// Correct where a group of fixtures hangs from the aims of its fixtures: the group the
    /// operator picks moves along the chosen axes, with the plot bar it hangs on. Only aims of
    /// that group's fixtures count, and only its fixtures move.
    fn rig_correction(&mut self, ui: &mut Ui, l: &mut Lighting) {
        let aims = self.hand_aims(l, None);
        let aimed = fixtures_of(aims.iter().map(|a| a.0));
        ui.horizontal(|ui| {
            ui.label(theme::caption(t("Correct the rig")));
            let all = self.aims();
            let list: Vec<String> = all
                .iter()
                .map(|(f, p, _)| format!("{} → {}", crate::light::model::fixture_label(*f), l.point(*p).map_or_else(|| p.to_string(), |p| if p.name.is_empty() { p.id.to_string() } else { p.name.clone() })))
                .collect();
            ui.label(RichText::new(tr!("{n} aimed", n = all.len().to_string())).color(theme::TEXT_DIM)).on_hover_text(list.join("\n"));
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
        let shown = |g: Option<GroupId>| g.and_then(|g| groups.iter().find(|x| x.0 == g)).map_or_else(|| t("Pick a group").to_string(), |x| x.1.clone());
        theme::wrapped_row(ui, |ui| {
            theme::keep_together(ui, "fix_group", |ui| {
                ui.label(t("Move")).on_hover_text(t("The fixtures that hang together (a truss): only this group moves, using the beams of its fixtures aimed by hand"));
                egui::ComboBox::from_id_salt("fix_group").selected_text(shown(self.light.fix_group)).show_ui(ui, |ui| {
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
            let group = self.light.fix_group.and_then(|g| l.groups.iter().find(|x| x.id == g)).map(|g| g.fixtures.clone());
            if ui
                .add_enabled(group.is_some(), egui::Button::new(theme::with_icon(icons::OPEN_WITH, t("Correct"))))
                .on_hover_text(t("Move the group's fixtures in the patch so their beams aimed by hand hit their points; every cue aimed at points follows, the points stay"))
                .clicked()
                && let Some(units) = group
            {
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
            if ui.button(theme::with_icon(icons::CLOSE, t("Aim at the points"))).on_hover_text(t("Put every fixture aimed from a point back on it (offsets to zero)")).clicked() {
                let units: Vec<FixtureId> = self.aims().iter().map(|a| a.0).collect();
                self.relink(&units);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::light::focus::move_from_point;
    use crate::light::model::Fixture;

    #[test]
    fn moving_a_referenced_fixture_keeps_the_point() {
        let mut l = Lighting::default();
        let t = crate::light::library::generics().into_iter().find(|t| t.physical.moving).expect("a generic mover");
        let mode = t.modes[0].name.clone();
        l.fixture_types.push(t.clone());
        l.fixtures = vec![Fixture { id: 1, fixture_type: t.id.clone(), mode, position: [3.0, 6.0, -2.0], ..Default::default() }].into();
        l.points.push(FocusPoint { id: 1, name: "Centre".into(), position: [0.0; 3] });
        let mut values = BTreeMap::from([(Attribute::Pan, Value::point(1)), (Attribute::Tilt, Value::point(1))]);
        let pan = l.resolve(1, Attribute::Pan, values[&Attribute::Pan]).unwrap();
        assert!(move_from_point(&l, 1, &mut values, Attribute::Pan, pan + 0.02));
        let Value::Point { point: 1, offset, .. } = values[&Attribute::Pan] else { panic!("{values:?}") };
        assert_ne!(offset, [0.0; 3]);
        assert_eq!(offset[1], 0.0, "stays on the point's height");
        assert_eq!(values[&Attribute::Tilt], values[&Attribute::Pan]);
        let moved = l.resolve(1, Attribute::Pan, values[&Attribute::Pan]).unwrap();
        assert!((moved - (pan + 0.02)).abs() < 1e-3, "{moved} vs {}", pan + 0.02);
        // The point moves: the fixture follows, the offset stays.
        l.points[0].position = [1.0, 0.0, 0.0];
        assert_ne!(l.resolve(1, Attribute::Pan, values[&Attribute::Pan]).unwrap(), moved);
        // Plain levels and other attributes are left alone.
        let mut plain = BTreeMap::from([(Attribute::Pan, Value::Level(0.5))]);
        assert!(!move_from_point(&l, 1, &mut plain, Attribute::Pan, 0.6));
        assert!(!move_from_point(&l, 1, &mut values, Attribute::Dimmer, 0.6));
    }
}
