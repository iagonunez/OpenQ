//! What each edit changed, so copies of the workspace kept elsewhere (the replicas' view on the
//! main device, the autosave) catch up by taking only the changed cues under the show lock.
//! Cloning a 50 000-cue show takes ~4 ms and comparing two copies ~8 ms (memory bound), which
//! held the engine off the lock once per replica per edit.
//!
//! [`Show::edited_cues`](crate::show::Show::edited_cues) tells the log which cues an edit
//! changed (their own fields only, nothing added, removed or moved);
//! [`Show::edited`](crate::show::Show::edited) says "anything may have changed". A [`Mirror`]
//! takes the changed cues plus the small parts of the workspace (settings, stages, list names,
//! the lighting design, whose big parts are shared) when every edit since its copy said which
//! cues it changed, and the whole workspace otherwise.

use crate::model::{Cue, CueId, CueList, OtherList, Workspace};
use std::collections::{HashSet, VecDeque};

/// Edits remembered; a mirror further behind takes the whole workspace.
const KEEP: usize = 256;

/// Which cues each recent edit generation changed ([`crate::show::Show::edit_gen`]).
#[derive(Default)]
pub struct ChangeLog {
    /// One entry per generation, oldest first: the cues changed, or None for "anything".
    entries: VecDeque<(u64, Option<Vec<CueId>>)>,
}

impl ChangeLog {
    /// Record generation `generation` (one more than the last one recorded).
    pub(crate) fn push(&mut self, generation: u64, cues: Option<Vec<CueId>>) {
        if self.entries.len() >= KEEP {
            self.entries.pop_front();
        }
        self.entries.push_back((generation, cues));
    }

    /// The cues changed after generation `since` up to `now`, when every edit in between said
    /// which; None when one didn't, or the log doesn't reach back that far.
    pub fn since(&self, since: u64, now: u64) -> Option<Vec<CueId>> {
        if now < since {
            return None;
        }
        let first = self.entries.front()?.0;
        if since + 1 < first {
            return None;
        }
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for (_, cues) in self.entries.iter().filter(|(g, _)| *g > since && *g <= now) {
            for id in cues.as_ref()? {
                if seen.insert(*id) {
                    out.push(*id);
                }
            }
        }
        Some(out)
    }
}

/// What a [`Mirror`] took from the show under the lock.
pub enum Taken {
    /// The changed cues (shallow copies, as [`Cue::shallow_clone`]) and the workspace without
    /// any cues ([`without_cues`]).
    Cues {
        /// Changed cues.
        cues: Vec<Cue>,
        /// Everything else.
        rest: Box<Workspace>,
        /// Generation taken.
        generation: u64,
    },
    /// A copy of the whole workspace.
    Whole(Box<Workspace>, u64),
}

/// How the copy changed ([`Mirror::catch_up`]).
#[derive(Debug)]
pub enum Delta {
    /// Not at all.
    Same,
    /// Only in these cues' own fields (shallow copies) and the lighting design.
    Patch(Vec<Cue>, Option<crate::light::patch::LightingPatch>),
    /// The copy was out of step and is gone: take again (the whole workspace).
    Lost,
    /// In its shape or settings: the whole copy is needed.
    Whole {
        /// There was a copy before, so the lighting design can go as a patch from it.
        had_copy: bool,
        /// The design's changes from the previous copy (when `had_copy`).
        lighting: Option<crate::light::patch::LightingPatch>,
    },
}

/// A copy of a show's workspace kept in step with it.
#[derive(Default)]
pub struct Mirror {
    ws: Option<Workspace>,
    generation: u64,
}

impl Mirror {
    /// The copy, once taken.
    pub fn ws(&self) -> Option<&Workspace> {
        self.ws.as_ref()
    }

    /// Forget the copy: the next [`Mirror::take`] takes the whole workspace.
    pub fn reset(&mut self) {
        self.ws = None;
    }

    /// Under the show lock: what changed since the copy, or None if nothing did. Cheap when
    /// the edits said which cues they changed.
    pub fn take(&self, show: &crate::show::Show) -> Option<Taken> {
        let now = show.edit_gen;
        if self.ws.is_none() {
            return Some(Taken::Whole(Box::new(show.ws.clone()), now));
        }
        if self.generation == now {
            return None;
        }
        let whole = || Some(Taken::Whole(Box::new(show.ws.clone()), now));
        let Some(ids) = show.changes.since(self.generation, now) else { return whole() };
        let mut cues = Vec::with_capacity(ids.len());
        for id in ids {
            match show.ws.cue_list.find(id) {
                Some(c) => cues.push(c.shallow_clone()),
                None => return whole(),
            }
        }
        Some(Taken::Cues { cues, rest: Box::new(without_cues(&show.ws)), generation: now })
    }

    /// Outside the lock: bring the copy up to date with what [`Mirror::take`] took.
    pub fn catch_up(&mut self, taken: Taken) -> Delta {
        match taken {
            Taken::Whole(ws, generation) => {
                self.generation = generation;
                let Some(old) = self.ws.replace(*ws) else { return Delta::Whole { had_copy: false, lighting: None } };
                let new = self.ws.as_ref().expect("just set");
                match crate::remote::cue_patch(&old, new) {
                    Some(cues) => {
                        let lighting = crate::light::patch::diff(&old.lighting, &new.lighting);
                        if cues.is_empty() && lighting.is_none() { Delta::Same } else { Delta::Patch(cues, lighting) }
                    }
                    None => Delta::Whole { had_copy: true, lighting: crate::light::patch::diff(&old.lighting, &new.lighting) },
                }
            }
            Taken::Cues { cues, rest, generation } => {
                let Some(copy) = self.ws.as_mut() else { return self.lost() };
                let mut changed = Vec::new();
                for c in cues {
                    match copy.cue_list.find_mut(c.id) {
                        Some(mine) if mine.is_group() == c.is_group() => {
                            if !mine.shallow_eq(&c) {
                                changed.push(c.shallow_clone());
                                mine.assign_shallow(c);
                            }
                        }
                        // The copy is out of step (it can't be when edits tell the truth).
                        _ => return self.lost(),
                    }
                }
                let rest = *rest;
                let lighting = crate::light::patch::diff(&copy.lighting, &rest.lighting);
                let same_rest = same_rest(copy, &rest);
                if !same_rest && copy.cue_list.others.len() != rest.cue_list.others.len() {
                    return self.lost();
                }
                copy.format_version = rest.format_version;
                copy.settings = rest.settings;
                copy.stages = rest.stages;
                copy.lighting = rest.lighting;
                copy.cue_list.info = rest.cue_list.info;
                copy.cue_list.pos = rest.cue_list.pos;
                for (mine, theirs) in copy.cue_list.others.iter_mut().zip(rest.cue_list.others) {
                    mine.info = theirs.info;
                }
                self.generation = generation;
                if !same_rest {
                    Delta::Whole { had_copy: true, lighting }
                } else if changed.is_empty() && lighting.is_none() {
                    Delta::Same
                } else {
                    Delta::Patch(changed, lighting)
                }
            }
        }
    }

    fn lost(&mut self) -> Delta {
        log::warn!("workspace copy out of step; taking the whole workspace");
        self.ws = None;
        Delta::Lost
    }
}

/// `ws` without any cues: settings, stages, list names and positions, the lighting design.
pub fn without_cues(ws: &Workspace) -> Workspace {
    let l = &ws.cue_list;
    Workspace {
        format_version: ws.format_version,
        cue_list: CueList {
            cues: Vec::new(),
            info: l.info.clone(),
            others: l.others.iter().map(|o| OtherList { info: o.info.clone(), cues: Vec::new() }).collect(),
            pos: l.pos,
            index: Default::default(),
        },
        settings: ws.settings.clone(),
        stages: ws.stages.clone(),
        lighting: ws.lighting.clone(),
    }
}

/// Same apart from the cues and the lighting design.
pub(crate) fn same_rest(a: &Workspace, b: &Workspace) -> bool {
    let (x, y) = (&a.cue_list, &b.cue_list);
    a.format_version == b.format_version
        && a.settings == b.settings
        && a.stages == b.stages
        && x.info == y.info
        && x.pos == y.pos
        && x.others.len() == y.others.len()
        && x.others.iter().zip(&y.others).all(|(p, q)| p.info == q.info)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CueKind, GroupMode};
    use crate::show::Show;

    fn show() -> Show {
        let mut show = Show::new(crate::media::Prober::new());
        let mut ws = Workspace::default();
        for i in 0..20 {
            let mut c = Cue::new(if i % 5 == 4 {
                CueKind::Group { mode: GroupMode::StartAll, children: vec![Cue::new(CueKind::Memo), Cue::new(CueKind::Memo)] }
            } else {
                CueKind::Wait { duration: 1.0 }
            });
            c.number = i.to_string();
            ws.cue_list.cues.push(c);
        }
        show.ws = ws;
        show.changed();
        show
    }

    /// Take and catch up; the copy must then equal the show.
    fn sync(m: &mut Mirror, show: &Show) -> Option<Delta> {
        let d = m.take(show).map(|t| m.catch_up(t));
        if !matches!(d, Some(Delta::Lost)) {
            assert!(m.ws() == Some(&show.ws), "copy out of step after {d:?}");
        }
        d
    }

    #[test]
    fn mirror_takes_only_the_changed_cues() {
        let mut show = show();
        let mut m = Mirror::default();
        assert!(matches!(sync(&mut m, &show), Some(Delta::Whole { had_copy: false, .. })));
        assert!(sync(&mut m, &show).is_none(), "nothing changed");

        let a = show.ws.cue_list.cues[3].id;
        let child = show.ws.cue_list.cues[4].children()[1].id;
        show.ws.cue_list.find_mut(a).unwrap().name = "a".into();
        show.edited_cues([a]);
        show.ws.cue_list.find_mut(child).unwrap().pre_wait = 2.0;
        show.edited_cues([child]);
        // Told but unchanged: not sent.
        show.edited_cues([show.ws.cue_list.cues[0].id]);
        match m.take(&show) {
            Some(Taken::Cues { cues, .. }) => assert_eq!(cues.len(), 3),
            _ => panic!("should take only the cues"),
        }
        match sync(&mut m, &show) {
            Some(Delta::Patch(cues, None)) => assert_eq!(cues.iter().map(|c| c.id).collect::<Vec<_>>(), [a, child]),
            d => panic!("{d:?}"),
        }

        // Settings change: the cues aren't taken, but the whole copy has to go.
        show.ws.settings.master_volume_db = -3.0;
        show.edited_outside_cues();
        assert!(matches!(m.take(&show), Some(Taken::Cues { .. })));
        assert!(matches!(sync(&mut m, &show), Some(Delta::Whole { had_copy: true, .. })));
        show.ws.lighting.button_scale = 1.5;
        show.edited_outside_cues();
        assert!(matches!(sync(&mut m, &show), Some(Delta::Patch(c, Some(_))) if c.is_empty()));

        // An edit that doesn't say what it changed: the whole workspace.
        show.ws.cue_list.cues.push(Cue::new(CueKind::Memo));
        show.edited();
        assert!(matches!(m.take(&show), Some(Taken::Whole(..))));
        assert!(matches!(sync(&mut m, &show), Some(Delta::Whole { had_copy: true, .. })));
        show.ws.cue_list.cues[1].name = "b".into();
        show.edited();
        assert!(matches!(sync(&mut m, &show), Some(Delta::Patch(c, None)) if c.len() == 1));

        // Further behind than the log reaches.
        for _ in 0..KEEP + 1 {
            show.edited_cues([a]);
        }
        assert!(matches!(m.take(&show), Some(Taken::Whole(..))));
        assert!(matches!(sync(&mut m, &show), Some(Delta::Same)));

        // A swapped workspace.
        show.replace_workspace(Workspace::default(), None);
        assert!(matches!(m.take(&show), Some(Taken::Whole(..))));
        sync(&mut m, &show);

        // An edit that tells a lie (a new cue as if it were only changed): the copy notices.
        let new = Cue::new(CueKind::Memo);
        let id = new.id;
        show.ws.cue_list.cues.push(new);
        show.edited_cues([id]);
        assert!(matches!(sync(&mut m, &show), Some(Delta::Lost)));
        assert!(matches!(sync(&mut m, &show), Some(Delta::Whole { had_copy: false, .. })));
    }

    #[test]
    fn change_log_spans() {
        let mut log = ChangeLog::default();
        let [a, b] = [CueId::new_v4(), CueId::new_v4()];
        log.push(1, Some(vec![a]));
        log.push(2, Some(vec![b, a]));
        log.push(3, None);
        log.push(4, Some(vec![]));
        assert_eq!(log.since(0, 2), Some(vec![a, b]));
        assert_eq!(log.since(1, 2), Some(vec![b, a]));
        assert_eq!(log.since(2, 3), None, "an edit that didn't say");
        assert_eq!(log.since(3, 4), Some(vec![]));
        assert_eq!(log.since(4, 4), Some(vec![]));
        let mut short = ChangeLog::default();
        short.push(10, Some(vec![a]));
        assert_eq!(short.since(8, 10), None, "the log doesn't reach back");
    }

    /// A replica's patch that half fits changes nothing; one that fits is applied, clamped.
    #[test]
    fn remote_patches_check_before_applying() {
        let mut show = show();
        let before = show.ws.clone();
        let mut known = show.ws.cue_list.cues[0].shallow_clone();
        known.pre_wait = f64::MAX;
        let unknown = Cue::new(CueKind::Memo);
        assert!(!show.apply_remote_patch(vec![known.clone(), unknown], None));
        assert!(show.ws == before);
        let group = show.ws.cue_list.cues[4].shallow_clone();
        assert!(show.apply_remote_patch(vec![known.clone(), group], None));
        let c = show.ws.cue_list.find(known.id).unwrap();
        assert_eq!(c.pre_wait, crate::limits::MAX_TIME, "brought into range");
        assert_eq!(show.ws.cue_list.cues[4].children().len(), 2, "a group keeps its children");
        assert_eq!(show.changes.since(show.edit_gen - 1, show.edit_gen).map(|v| v.len()), Some(2));
    }

    #[test]
    fn lookups_follow_edits() {
        let mut show = show();
        let first = show.ws.cue_list.cues[1].id;
        show.ws.cue_list.cues[2].number = "1".into();
        show.edited();
        assert_eq!(show.lookups().numbers.get("1"), Some(&first), "the first cue with a number");
        show.ws.cue_list.cues[1].number = "x".into();
        show.edited_cues([first]);
        assert_eq!(show.lookups().numbers.get("x"), Some(&first));
        assert_eq!(show.lookups().numbers.get("1"), Some(&show.ws.cue_list.cues[2].id));
        assert!(show.lookups().media.is_empty());
    }
}
