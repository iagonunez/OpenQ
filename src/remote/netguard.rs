//! Network settings in a replica's edit wait for the main device's operator.
//!
//! Where the show sends to or listens on (Network cue destinations, lighting DMX outputs and
//! inputs) decides what the main device does on its network. A replica's edit that changes any of
//! it is taken without those changes ([`hold_workspace`], [`hold_cues`], [`hold_lighting`]); the
//! changes become a [`NetworkRequest`] that the operator allows ([`NetworkChange::apply`]) or
//! denies on the main device. The replica gets the show back as it is meanwhile.

use crate::light::dmx::{OutputConfig, OutputProtocol};
use crate::light::dmxin::InputConfig;
use crate::light::patch::LightingPatch;
use crate::model::{Cue, CueId, CueKind, Workspace};

/// A network cue's destination change held back.
#[derive(Clone, Debug, PartialEq)]
pub struct HeldCue {
    /// The cue.
    pub id: CueId,
    /// Its number and name, for the operator.
    pub label: String,
    /// Host and port before (None: a new cue).
    pub before: Option<(String, u16)>,
    /// Host and port wanted.
    pub wanted: (String, u16),
}

/// Network settings changes held back from one edit.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NetworkChange {
    /// Network cues whose destination changed.
    pub cues: Vec<HeldCue>,
    /// Wanted DMX outputs (whole list), if they changed.
    pub outputs: Option<Vec<OutputConfig>>,
    /// Wanted DMX inputs (whole list), if they changed.
    pub inputs: Option<Vec<InputConfig>>,
}

/// A held change waiting for the operator.
#[derive(Clone, Debug, PartialEq)]
pub struct NetworkRequest {
    /// Number of the request (for the UI).
    pub id: u64,
    /// Name of the device that asked.
    pub device: String,
    /// What it wants.
    pub change: NetworkChange,
}

/// Requests kept waiting at most (older ones are dropped).
pub const MAX_REQUESTS: usize = 16;

impl NetworkChange {
    /// Nothing held.
    pub fn is_empty(&self) -> bool {
        self.cues.is_empty() && self.outputs.is_none() && self.inputs.is_none()
    }

    /// Take the changes into `ws` (allowed by the operator). Cues deleted since are skipped.
    pub fn apply(&self, ws: &mut Workspace) {
        for h in &self.cues {
            if let Some(Cue { kind: CueKind::Network(n), .. }) = ws.cue_list.find_mut(h.id) {
                (n.host, n.port) = h.wanted.clone();
            }
        }
        if let Some(o) = &self.outputs {
            ws.lighting.outputs = o.clone();
        }
        if let Some(i) = &self.inputs {
            ws.lighting.inputs = i.clone();
        }
    }

    /// One line per change, for the operator.
    pub fn describe(&self) -> Vec<String> {
        let dest = |d: &(String, u16)| format!("{}:{}", if d.0.is_empty() { "?" } else { &d.0 }, d.1);
        let mut out: Vec<String> = self
            .cues
            .iter()
            .map(|h| match &h.before {
                Some(b) => crate::tr!("Network cue {cue}: {from} → {to}", cue = h.label.as_str(), from = dest(b), to = dest(&h.wanted)),
                None => crate::tr!("New network cue {cue} sending to {to}", cue = h.label.as_str(), to = dest(&h.wanted)),
            })
            .collect();
        if let Some(o) = &self.outputs {
            let list: Vec<String> = o.iter().map(describe_output).collect();
            out.push(crate::tr!("DMX outputs: {list}", list = if list.is_empty() { "–".to_string() } else { list.join(", ") }));
        }
        if let Some(i) = &self.inputs {
            let list: Vec<String> = i
                .iter()
                .map(|i| {
                    let on = if i.enabled { "" } else { " (off)" };
                    let at = if i.interface.is_empty() { String::new() } else { format!(" @ {}", i.interface) };
                    format!("{} {}{at}{on}", i.name, i.protocol.label())
                })
                .collect();
            out.push(crate::tr!("DMX inputs: {list}", list = if list.is_empty() { "–".to_string() } else { list.join(", ") }));
        }
        // Hosts and names come from the replica: no control characters, nothing endless.
        out.into_iter().map(|l| l.chars().filter(|c| !c.is_control()).take(400).collect()).collect()
    }
}

fn describe_output(o: &OutputConfig) -> String {
    let to = match &o.protocol {
        OutputProtocol::ArtNet { target, .. } => if target.is_empty() { "broadcast".to_string() } else { target.clone() },
        OutputProtocol::Sacn { target, .. } => if target.is_empty() { "multicast".to_string() } else { target.clone() },
        OutputProtocol::EnttecPro { port } => port.clone(),
    };
    let on = if o.enabled { "" } else { " (off)" };
    let kind = o.protocol.label();
    let name = o.name.trim();
    if name.is_empty() || name == kind { format!("{kind} → {to}{on}") } else { format!("{name} ({kind}) → {to}{on}") }
}

fn label(c: &Cue) -> String {
    match (c.number.trim(), c.name.trim()) {
        ("", "") => "–".into(),
        (n, "") => n.into(),
        ("", name) => format!("\u{201c}{name}\u{201d}"),
        (n, name) => format!("{n} \u{201c}{name}\u{201d}"),
    }
}

/// Destination of a network cue.
fn dest(c: &Cue) -> Option<(String, u16)> {
    match &c.kind {
        CueKind::Network(n) => Some((n.host.clone(), n.port)),
        _ => None,
    }
}

/// If `c` is a network cue whose destination differs from the current one, put back the current
/// destination (none for a new cue) and note the wanted one in `change`.
fn hold(current: &Workspace, c: &mut Cue, change: &mut NetworkChange) {
    let (id, text) = (c.id, label(c));
    let CueKind::Network(n) = &mut c.kind else { return };
    let before = current.cue_list.find(id).and_then(dest);
    let wanted = (n.host.clone(), n.port);
    if before.as_ref() == Some(&wanted) {
        return;
    }
    match &before {
        Some((host, port)) => (n.host, n.port) = (host.clone(), *port),
        // A new cue sends nowhere until the operator allows it.
        None => n.host = String::new(),
    }
    change.cues.push(HeldCue { id, label: text, before, wanted });
}

/// Hold back the network settings changes of a whole workspace from a replica.
pub fn hold_workspace(current: &Workspace, new: &mut Workspace, change: &mut NetworkChange) {
    fn walk(current: &Workspace, list: &mut [Cue], change: &mut NetworkChange) {
        for c in list {
            hold(current, c, change);
            if let CueKind::Group { children, .. } = &mut c.kind {
                walk(current, children, change);
            }
        }
    }
    for list in new.cue_list.all_roots_mut() {
        walk(current, list, change);
    }
    if new.lighting.outputs != current.lighting.outputs {
        change.outputs = Some(std::mem::replace(&mut new.lighting.outputs, current.lighting.outputs.clone()));
    }
    if new.lighting.inputs != current.lighting.inputs {
        change.inputs = Some(std::mem::replace(&mut new.lighting.inputs, current.lighting.inputs.clone()));
    }
}

/// Hold back the network settings changes of changed cues from a replica.
pub fn hold_cues(current: &Workspace, cues: &mut [Cue], change: &mut NetworkChange) {
    for c in cues {
        hold(current, c, change);
    }
}

/// Hold back the DMX output and input changes of a lighting patch from a replica.
pub fn hold_lighting(current: &Workspace, patch: &mut LightingPatch, change: &mut NetworkChange) {
    if let Some(o) = patch.outputs.take()
        && o != current.lighting.outputs
    {
        change.outputs = Some(o);
    }
    if let Some(i) = patch.inputs.take()
        && i != current.lighting.inputs
    {
        change.inputs = Some(i);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::NetworkCue;

    #[test]
    fn network_changes_wait() {
        let mut ws = Workspace::default();
        let net = Cue::new(CueKind::Network(NetworkCue { host: "10.0.0.5".into(), ..Default::default() }));
        ws.cue_list.cues.push(net.clone());

        // A renamed cue goes through; a new destination and new outputs are held.
        let mut new = ws.clone();
        new.cue_list.cues[0].name = "Console".into();
        if let CueKind::Network(n) = &mut new.cue_list.cues[0].kind {
            n.host = "203.0.113.9".into();
        }
        let mut added = Cue::new(CueKind::Network(NetworkCue { host: "198.51.100.1".into(), ..Default::default() }));
        added.number = "7".into();
        new.cue_list.cues.push(added.clone());
        new.lighting.outputs.push(OutputConfig::default());
        let mut change = NetworkChange::default();
        hold_workspace(&ws, &mut new, &mut change);
        assert_eq!(change.cues.len(), 2);
        assert!(change.outputs.is_some() && change.inputs.is_none());
        assert_eq!(new.cue_list.cues[0].name, "Console");
        assert_eq!(dest(&new.cue_list.cues[0]).unwrap().0, "10.0.0.5", "destination kept");
        assert_eq!(dest(&new.cue_list.cues[1]).unwrap().0, "", "a new cue sends nowhere yet");
        assert!(new.lighting.outputs.is_empty());
        assert_eq!(change.describe().len(), 3);

        // Allowed: the wanted values land in the show.
        change.apply(&mut new);
        assert_eq!(dest(&new.cue_list.cues[0]).unwrap().0, "203.0.113.9");
        assert_eq!(dest(new.cue_list.find(added.id).unwrap()).unwrap().0, "198.51.100.1");
        assert_eq!(new.lighting.outputs.len(), 1);

        // Nothing network-related: nothing held.
        let mut same = ws.clone();
        same.cue_list.cues[0].pre_wait = 1.0;
        let mut change = NetworkChange::default();
        hold_workspace(&ws, &mut same, &mut change);
        assert!(change.is_empty());

        // Patches: cues and lighting.
        let mut cue = net.clone();
        if let CueKind::Network(n) = &mut cue.kind {
            n.port = 9000;
        }
        let mut cues = vec![cue];
        let mut patch = LightingPatch { outputs: Some(vec![OutputConfig::default()]), ..Default::default() };
        let mut change = NetworkChange::default();
        hold_cues(&ws, &mut cues, &mut change);
        hold_lighting(&ws, &mut patch, &mut change);
        assert_eq!(dest(&cues[0]).unwrap().1, 53000);
        assert!(patch.outputs.is_none() && change.outputs.is_some() && change.cues.len() == 1);
    }
}
