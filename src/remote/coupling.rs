//! Coupling mode: several computers hold the same show and, if the one playing it stops, the next
//! one in a priority list takes over.
//!
//! Computers couple with one code, not by pairing every two of them: the first one creates a
//! group (a random key), each other joins with a short pairing code shown on a computer already in
//! it ([`Grant`]), and from then on members log in to each other with the group key. The priority
//! list travels with the group ([`Roster`]): any member may change it and the others adopt the
//! newest version when they next ask. Everybody holding the key is trusted alike, and a member
//! taken out of the list still holds it: to lock one out for good, start a new group.
//!
//! Every member keeps its remote server on. The *main* device plays the show; the others are
//! *standby* devices: ordinary replicas that mirror its workspace and what is running
//! ([`crate::remote::ServerMsg::State`]). A standby device refuses other devices' connections
//! (they find the main one instead). When a standby device loses its main device it does not
//! take over at once: it first asks the other members what they are doing ([`probe`], a
//! handshake that only reads [`Presence`]) and decides with [`decide`]:
//!
//! * a member that is acting as main is followed (the one with the highest `term`; the better
//!   priority wins a tie);
//! * otherwise it takes over only if every member *above* it in the list is unreachable (or a
//!   standby device that has itself been without a main for too long, so a frozen computer
//!   can't hold the show up). A member that is alive blocks the ones below it: it takes over
//!   itself. That is also why a device that only lost *its own* connection to the main device
//!   never splits the show in two when a better standby device is still following it;
//! * a restarted member never takes the show back by itself: it joins whoever is main now.
//!   "Take over now" (a button) starts a new `term`, and the previous main device steps down
//!   when it sees it. A main device that finds another with a higher term (a partition that
//!   healed) steps down the same way.
//!
//! A new main device restarts what the mirror said was running, at the point it had reached
//! ([`crate::engine::Engine::adopt`]) and its lighting masters and sequence positions. It
//! plays from its own copy of the media files, so those must be at the same paths on every
//! member (File → Bundle show copies them). The decision logic here is pure; the UI side
//! (`ui::remote`) carries it out.

use super::client;
use super::RemoteConfig;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// How often every member is asked what it is doing.
const ROUND_PAUSE: Duration = Duration::from_millis(500);
/// Longest one question may take (connecting, handshake and answer).
const PROBE_TIME: Duration = Duration::from_millis(1500);
/// A standby device without a main device for this many times the silence limit is not going
/// to take over (it is hung, or cut off): the members below it stop waiting for it.
const STALE_FACTOR: f64 = 2.0;
/// Most members in a list.
pub const MAX_MEMBERS: usize = 8;

/// A computer in the priority list.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Member {
    /// Its remote instance id.
    pub id: Uuid,
    /// Its name.
    pub name: String,
    /// Where to reach it (`host:port`).
    pub address: String,
}

/// The secret a coupling group's members share.
#[derive(Serialize, Deserialize, Clone, PartialEq)]
pub struct Group {
    /// Names the group (not secret): a computer of another group is told so.
    pub id: Uuid,
    /// Random key (hex). Never shown, logged or put in a [`Presence`].
    pub key: String,
}

impl std::fmt::Debug for Group {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Group({})", self.id)
    }
}

impl Group {
    /// A new group with a random key.
    pub fn create() -> Self {
        Self { id: Uuid::new_v4(), key: super::new_token() }
    }

    /// What members prove to each other (the handshake's shared secret).
    pub fn secret(&self) -> Vec<u8> {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(b"opnq coupling group v1\0");
        h.update(self.id.as_bytes());
        h.update(self.key.as_bytes());
        h.finalize().iter().map(|b| format!("{b:02x}")).collect::<String>().into_bytes()
    }
}

/// The priority list as one version of it: the highest `rev` wins.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Roster {
    /// Version, raised by every change.
    pub rev: u64,
    /// Computers in priority order.
    pub members: Vec<Member>,
}

/// What a computer that joins a group is handed (over the pairing code's encrypted channel).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Grant {
    /// The group's id.
    pub id: Uuid,
    /// The group's key.
    pub key: String,
    /// The priority list, the new computer last.
    pub roster: Roster,
    /// Seconds before a silent main device counts as gone.
    pub silence: f64,
}

/// What [`Coupling::adopt`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Adopted {
    /// Nothing changed.
    Same,
    /// The list changed.
    Changed,
    /// The list changed and this computer is no longer in it.
    Removed,
}

/// The coupling settings of this computer (in the remote config, not in the show).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Coupling {
    /// Take part: keep the server on and follow the priority list.
    pub enabled: bool,
    /// The group this computer belongs to.
    pub group: Option<Group>,
    /// Version of `members`.
    pub rev: u64,
    /// Computers in priority order, this one included (the first plays the show).
    pub members: Vec<Member>,
    /// Seconds without a word from the main device before it counts as gone.
    pub silence: f64,
    /// Folder of the show package this computer plays its media from, if any
    /// ([`crate::package`]): media paths of the mirrored show are found there when it takes over.
    pub media_root: Option<std::path::PathBuf>,
}

impl Default for Coupling {
    fn default() -> Self {
        Self { enabled: false, group: None, rev: 0, members: Vec::new(), silence: 2.0, media_root: None }
    }
}

impl Coupling {
    /// Position of `id` in the list (0 = first choice).
    pub fn rank(&self, id: Uuid) -> Option<usize> {
        self.members.iter().position(|m| m.id == id)
    }

    /// Coupling is on and this computer is in a group's list with at least one other.
    pub fn active_for(&self, me: Uuid) -> bool {
        self.enabled && self.group.is_some() && self.members.len() >= 2 && self.rank(me).is_some()
    }

    /// The list as a versioned [`Roster`].
    pub fn roster(&self) -> Roster {
        Roster { rev: self.rev, members: self.members.clone() }
    }

    /// Start a group with this computer as its only (first) member.
    pub fn create(&mut self, me: Member) {
        self.group = Some(Group::create());
        self.members = vec![me];
        self.rev = 1;
        self.enabled = true;
    }

    /// Take the grant of the group joined.
    pub fn join(&mut self, g: Grant) {
        self.group = Some(Group { id: g.id, key: g.key });
        self.members = g.roster.members;
        self.rev = g.roster.rev;
        self.silence = g.silence;
        self.enabled = true;
        self.clean();
    }

    /// Leave the group: forget the key and the list.
    pub fn leave(&mut self) {
        self.group = None;
        self.members.clear();
        self.rev = 0;
        self.enabled = false;
    }

    /// The list was changed here: make it the newest version.
    pub fn edited(&mut self) {
        self.rev = self.rev.saturating_add(1);
    }

    /// Take another member's list if it is newer (or, at the same version, if it sorts first, so
    /// that members who changed it at once still end up with one). Lists are cleaned first.
    pub fn adopt(&mut self, me: Uuid, mut r: Roster) -> Adopted {
        // (A version this high can only be made up; taking it would stop every later change.)
        if r.rev > u64::MAX / 2 {
            return Adopted::Same;
        }
        let mut tidy = Coupling { members: std::mem::take(&mut r.members), ..Default::default() };
        tidy.clean();
        let key = |m: &[Member]| serde_json::to_string(m).unwrap_or_default();
        let newer = r.rev > self.rev || (r.rev == self.rev && key(&tidy.members) < key(&self.members));
        if !newer || tidy.members == self.members {
            if r.rev > self.rev {
                self.rev = r.rev;
            }
            return Adopted::Same;
        }
        self.rev = r.rev;
        self.members = tidy.members;
        if self.rank(me).is_some() { Adopted::Changed } else { Adopted::Removed }
    }

    /// The silence limit, kept to a sensible range.
    pub fn silence_limit(&self) -> Duration {
        Duration::from_secs_f64(if self.silence.is_finite() { self.silence.clamp(1.0, 30.0) } else { 2.0 })
    }

    /// Bring the settings into range (a damaged or hand-edited config).
    pub fn clean(&mut self) {
        let mut seen = std::collections::HashSet::new();
        self.members.retain(|m| seen.insert(m.id));
        self.members.truncate(MAX_MEMBERS);
        for m in &mut self.members {
            m.name = m.name.chars().filter(|c| !c.is_control()).take(64).collect();
            m.address = m.address.chars().filter(|c| !c.is_control()).take(256).collect();
        }
        if !self.silence.is_finite() {
            self.silence = 2.0;
        }
        self.silence = self.silence.clamp(1.0, 30.0);
    }
}

/// What a device tells the others about itself ([`crate::remote::ServerMsg::Welcome`]).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Presence {
    /// It plays the show (it is the main device).
    pub acting: bool,
    /// Generation of the main device: it grows with every takeover. A standby device reports
    /// the last one it saw.
    pub term: u64,
    /// For a device without a main device: seconds it has been without one.
    pub orphan_secs: f64,
    /// The priority list, only for a member of the group asking.
    #[serde(default)]
    pub roster: Option<Roster>,
}

/// A device's own coupling state, kept by its server to answer with a [`Presence`].
#[derive(Clone, Copy, Debug)]
pub struct Local {
    /// Plays the show.
    pub acting: bool,
    /// Generation, see [`Presence::term`].
    pub term: u64,
    /// Since when it has had no main device (standby only).
    pub orphan_since: Option<Instant>,
}

impl Local {
    /// What to tell a peer.
    pub fn presence(&self) -> Presence {
        Presence { acting: self.acting, term: self.term, orphan_secs: self.orphan_since.map_or(0.0, |t| t.elapsed().as_secs_f64()), roster: None }
    }
}

/// What one probe found out about another member.
#[derive(Clone, Debug, PartialEq)]
pub enum Peer {
    /// Not asked yet.
    Unknown,
    /// Nobody answered (or it isn't running coupling).
    Unreachable(String),
    /// It answered but refused us (not paired in this direction, other protocol...).
    NotPaired(String),
    /// Alive, not playing.
    Standby {
        /// Seconds it has been without a main device (0: it follows one).
        orphan_secs: f64,
        /// The last term it saw.
        term: u64,
    },
    /// Alive and playing the show.
    Main {
        /// Its generation.
        term: u64,
    },
}

/// What this device is doing.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Duty {
    /// Playing the show.
    Main {
        /// Generation.
        term: u64,
    },
    /// Mirroring this member.
    Following {
        /// The main device.
        main: Uuid,
    },
    /// Without a main device.
    Orphan {
        /// Since when.
        since: Instant,
        /// How long to wait before taking over at all (a fresh start waits for the others to
        /// show up; losing a main device does not).
        grace: Duration,
        /// The last term seen.
        term: u64,
    },
}

/// What [`decide`] says to do.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Verdict {
    /// Nothing.
    Wait,
    /// Connect to this member (it is the main device).
    Follow(Uuid),
    /// Become the main device, with this term.
    TakeOver(u64),
}

/// The better claim to being the main device: higher term, then better priority.
fn beats(term: u64, rank: usize, other_term: u64, other_rank: usize) -> bool {
    term > other_term || (term == other_term && rank < other_rank)
}

/// The member playing the show with the best claim.
fn best_main(fo: &Coupling, me: Uuid, peers: &HashMap<Uuid, Peer>) -> Option<(Uuid, u64, usize)> {
    let mut best: Option<(Uuid, u64, usize)> = None;
    for (rank, m) in fo.members.iter().enumerate() {
        if m.id == me {
            continue;
        }
        if let Some(Peer::Main { term }) = peers.get(&m.id)
            && best.is_none_or(|(_, t, r)| beats(*term, rank, t, r))
        {
            best = Some((m.id, *term, rank));
        }
    }
    best
}

/// What a device should do, given what its peers said. Pure: nothing here talks to the
/// network or reads the clock (`now` is passed in).
pub fn decide(fo: &Coupling, me: Uuid, duty: &Duty, peers: &HashMap<Uuid, Peer>, now: Instant) -> Verdict {
    let Some(my_rank) = fo.rank(me) else { return Verdict::Wait };
    let best = best_main(fo, me, peers);
    match *duty {
        Duty::Main { term } => match best {
            // Someone else took over (a partition healed, or "take over now" there).
            Some((id, t, r)) if beats(t, r, term, my_rank) => Verdict::Follow(id),
            _ => Verdict::Wait,
        },
        Duty::Following { main } => {
            let followed = fo.rank(main).zip(match peers.get(&main) {
                Some(Peer::Main { term }) => Some(*term),
                _ => None,
            });
            match (best, followed) {
                (Some((id, t, r)), Some((fr, ft))) if id != main && beats(t, r, ft, fr) => Verdict::Follow(id),
                // The one we follow isn't answering as main (its link will tell if it's gone).
                _ => Verdict::Wait,
            }
        }
        Duty::Orphan { since, grace, term } => {
            if let Some((id, ..)) = best {
                return Verdict::Follow(id);
            }
            if now.saturating_duration_since(since) < grace {
                return Verdict::Wait;
            }
            let stale = fo.silence_limit().as_secs_f64() * STALE_FACTOR;
            let mut highest = term;
            for (rank, m) in fo.members.iter().enumerate() {
                if m.id == me {
                    continue;
                }
                match peers.get(&m.id) {
                    // Not asked yet: wait for the answer.
                    None | Some(Peer::Unknown) => return Verdict::Wait,
                    Some(Peer::Standby { orphan_secs, term }) => {
                        highest = highest.max(*term);
                        // Alive and better placed (or following a main device we can't reach):
                        // it takes over, or knows better.
                        if rank < my_rank && *orphan_secs < stale {
                            return Verdict::Wait;
                        }
                    }
                    Some(Peer::Main { term }) => highest = highest.max(*term),
                    Some(Peer::Unreachable(_) | Peer::NotPaired(_)) => {}
                }
            }
            Verdict::TakeOver(highest + 1)
        }
    }
}

/// Ask a member what it is doing. Blocks up to `limit`.
pub fn probe(config: &Arc<Mutex<RemoteConfig>>, member: &Member, limit: Duration) -> (Peer, Option<Roster>) {
    client::probe(config, member.id, &member.address, limit)
}

struct Input {
    coupling: Coupling,
    me: Uuid,
    duty: Duty,
    /// When `duty` last changed: answers older than this are about something else.
    duty_since: Instant,
}

struct Output {
    peers: HashMap<Uuid, Peer>,
    /// The newest priority list the members reported.
    roster: Option<Roster>,
    /// The verdict of the last round, with the `duty_since` it was made for.
    verdict: Option<(Instant, Verdict)>,
    rounds: u64,
}

struct Inner {
    config: Arc<Mutex<RemoteConfig>>,
    input: Mutex<Input>,
    output: Mutex<Output>,
    stop: AtomicBool,
}

/// Asks the other members what they are doing, round after round on its own thread (so a
/// frozen window can't stop the others from deciding), and turns the answers into a [`Verdict`].
pub struct Supervisor {
    inner: Arc<Inner>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Supervisor {
    /// Start asking. `repaint` is called whenever a round ends.
    pub fn start(config: Arc<Mutex<RemoteConfig>>, repaint: impl Fn() + Send + 'static) -> Self {
        let (coupling, me) = {
            let c = config.lock();
            (c.coupling.clone(), c.instance_id)
        };
        let inner = Arc::new(Inner {
            config,
            input: Mutex::new(Input { coupling, me, duty: Duty::Orphan { since: Instant::now(), grace: Duration::ZERO, term: 0 }, duty_since: Instant::now() }),
            output: Mutex::new(Output { peers: HashMap::new(), roster: None, verdict: None, rounds: 0 }),
            stop: AtomicBool::new(false),
        });
        let sh = inner.clone();
        let thread = std::thread::Builder::new()
            .name("opnq-coupling".into())
            .spawn(move || {
                while !sh.stop.load(Ordering::Relaxed) {
                    let _ = crate::recovery::catch(|| {
                        round(&sh, &repaint);
                        Ok(())
                    });
                    let until = Instant::now() + ROUND_PAUSE;
                    while Instant::now() < until && !sh.stop.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(25));
                    }
                }
            })
            .ok();
        Self { inner, thread }
    }

    /// Tell it what this device is doing now (and the list, if it changed).
    pub fn set(&self, coupling: &Coupling, duty: Duty) {
        let mut i = self.inner.input.lock();
        if i.coupling != *coupling {
            i.coupling = coupling.clone();
        }
        // Same kind of duty with the same details: nothing new to decide about.
        if i.duty != duty {
            let kind = |d: &Duty| std::mem::discriminant(d);
            let changed = kind(&i.duty) != kind(&duty)
                || matches!((&i.duty, &duty), (Duty::Following { main: a }, Duty::Following { main: b }) if a != b)
                || matches!((&i.duty, &duty), (Duty::Main { term: a }, Duty::Main { term: b }) if a != b);
            i.duty = duty;
            if changed {
                i.duty_since = Instant::now();
            }
        }
    }

    /// What to do now, if the last round was made after the duty last changed.
    pub fn verdict(&self) -> Verdict {
        let since = self.inner.input.lock().duty_since;
        match self.inner.output.lock().verdict {
            Some((at, v)) if at >= since => v,
            _ => Verdict::Wait,
        }
    }

    /// What the last round found out about each member.
    pub fn peers(&self) -> HashMap<Uuid, Peer> {
        self.inner.output.lock().peers.clone()
    }

    /// The newest priority list a member reported (to adopt with [`Coupling::adopt`]).
    pub fn roster(&self) -> Option<Roster> {
        self.inner.output.lock().roster.clone()
    }

    /// Rounds finished so far.
    pub fn rounds(&self) -> u64 {
        self.inner.output.lock().rounds
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        self.inner.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// One round: ask everyone at once, wait for the answers (or the limit), decide.
fn round(sh: &Arc<Inner>, repaint: &impl Fn()) {
    let (coupling, me, since) = {
        let i = sh.input.lock();
        (i.coupling.clone(), i.me, i.duty_since)
    };
    let started = Instant::now();
    let limit = PROBE_TIME;
    let (tx, rx) = std::sync::mpsc::channel();
    let mut asked = 0;
    for m in coupling.members.iter().filter(|m| m.id != me && !m.address.trim().is_empty()) {
        let (tx, config, m) = (tx.clone(), sh.config.clone(), m.clone());
        // (A name that never resolves can hold its thread past the limit; the round doesn't wait.)
        if std::thread::Builder::new()
            .name(crate::recovery::thread_name("opnq-probe", &m.name))
            .spawn(move || {
                let r = probe(&config, &m, limit);
                let _ = tx.send((m.id, r.0, r.1));
            })
            .is_ok()
        {
            asked += 1;
        }
    }
    drop(tx);
    let mut peers: HashMap<Uuid, Peer> = coupling.members.iter().filter(|m| m.id != me).map(|m| (m.id, Peer::Unreachable("no address".into()))).collect();
    let deadline = started + limit + Duration::from_millis(500);
    let mut rosters: Vec<Roster> = Vec::new();
    for _ in 0..asked {
        let Some(wait) = deadline.checked_duration_since(Instant::now()) else { break };
        match rx.recv_timeout(wait) {
            Ok((id, p, roster)) => {
                peers.insert(id, p);
                rosters.extend(roster);
            }
            Err(_) => break,
        }
    }
    let (duty, duty_since) = {
        let i = sh.input.lock();
        (i.duty, i.duty_since)
    };
    // The duty changed while asking: those answers may be about the old situation.
    let verdict = if duty_since == since { decide(&coupling, me, &duty, &peers, Instant::now()) } else { Verdict::Wait };
    {
        let mut o = sh.output.lock();
        o.peers = peers;
        // The newest list, and at one version the one that sorts first (as `Coupling::adopt`).
        let key = |r: &Roster| serde_json::to_string(&r.members).unwrap_or_default();
        o.roster = rosters.into_iter().min_by(|a, b| b.rev.cmp(&a.rev).then_with(|| key(a).cmp(&key(b))));
        o.verdict = Some((started, verdict));
        o.rounds += 1;
    }
    repaint();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids() -> [Uuid; 4] {
        [Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3), Uuid::from_u128(4)]
    }

    fn list(n: usize) -> Coupling {
        let ids = ids();
        Coupling {
            enabled: true,
            group: Some(Group::create()),
            rev: 1,
            members: (0..n).map(|i| Member { id: ids[i], name: format!("pc{i}"), address: format!("10.0.0.{i}:53200") }).collect(),
            silence: 2.0,
            media_root: None,
        }
    }

    fn orphan(secs_ago: u64, grace: Duration) -> (Duty, Instant) {
        let now = Instant::now();
        (Duty::Orphan { since: now - Duration::from_secs(secs_ago), grace, term: 3 }, now)
    }

    fn peers(items: &[(usize, Peer)]) -> HashMap<Uuid, Peer> {
        let ids = ids();
        items.iter().map(|(i, p)| (ids[*i], p.clone())).collect()
    }

    fn gone() -> Peer {
        Peer::Unreachable("down".into())
    }

    #[test]
    fn the_next_one_takes_over_when_the_main_is_gone() {
        let fo = list(3);
        let (duty, now) = orphan(1, Duration::ZERO);
        let p = peers(&[(0, gone()), (2, Peer::Standby { orphan_secs: 1.0, term: 3 })]);
        // Rank 1: everything above it is down.
        assert_eq!(decide(&fo, ids()[1], &duty, &p, now), Verdict::TakeOver(4));
        // Rank 2: rank 1 is alive and will take over.
        let p = peers(&[(0, gone()), (1, Peer::Standby { orphan_secs: 1.0, term: 3 })]);
        assert_eq!(decide(&fo, ids()[2], &duty, &p, now), Verdict::Wait);
    }

    #[test]
    fn a_standby_that_never_takes_over_is_not_waited_for() {
        let fo = list(3);
        let (duty, now) = orphan(6, Duration::ZERO);
        let p = peers(&[(0, gone()), (1, Peer::Standby { orphan_secs: 30.0, term: 3 })]);
        assert_eq!(decide(&fo, ids()[2], &duty, &p, now), Verdict::TakeOver(4));
    }

    #[test]
    fn a_better_placed_member_that_still_follows_blocks() {
        // Cut off from the main device while the member above still follows it.
        let fo = list(3);
        let (duty, now) = orphan(10, Duration::ZERO);
        let p = peers(&[(0, Peer::Main { term: 3 }), (1, Peer::Standby { orphan_secs: 0.0, term: 3 })]);
        // The main device answers us: follow it, don't take over.
        assert_eq!(decide(&fo, ids()[2], &duty, &p, now), Verdict::Follow(ids()[0]));
        let p = peers(&[(0, gone()), (1, Peer::Standby { orphan_secs: 0.0, term: 3 })]);
        assert_eq!(decide(&fo, ids()[2], &duty, &p, now), Verdict::Wait);
    }

    #[test]
    fn nobody_takes_over_before_the_answers_are_in_or_the_grace_is_over() {
        let fo = list(2);
        let (duty, now) = orphan(1, Duration::from_secs(5));
        let p = peers(&[(0, gone())]);
        assert_eq!(decide(&fo, ids()[1], &duty, &p, now), Verdict::Wait);
        let (duty, now) = orphan(6, Duration::from_secs(5));
        assert_eq!(decide(&fo, ids()[1], &duty, &HashMap::new(), now), Verdict::Wait);
        assert_eq!(decide(&fo, ids()[1], &duty, &p, now), Verdict::TakeOver(4));
    }

    #[test]
    fn a_member_that_comes_back_joins_the_main_one_whatever_its_place() {
        let fo = list(3);
        let (duty, now) = orphan(0, Duration::from_secs(3));
        // Rank 0 restarts while rank 1 has been playing.
        let p = peers(&[(1, Peer::Main { term: 4 }), (2, Peer::Standby { orphan_secs: 0.0, term: 4 })]);
        assert_eq!(decide(&fo, ids()[0], &duty, &p, now), Verdict::Follow(ids()[1]));
    }

    #[test]
    fn the_highest_term_wins_between_two_mains() {
        let fo = list(3);
        let (a, b, c) = (ids()[0], ids()[1], ids()[2]);
        // Rank 0 plays on term 3; rank 1 took over (term 4) while they couldn't see each other.
        let p = peers(&[(1, Peer::Main { term: 4 })]);
        assert_eq!(decide(&fo, a, &Duty::Main { term: 3 }, &p, Instant::now()), Verdict::Follow(b));
        // The other way round, nothing to do for the newer one.
        let p = peers(&[(0, Peer::Main { term: 3 })]);
        assert_eq!(decide(&fo, b, &Duty::Main { term: 4 }, &p, Instant::now()), Verdict::Wait);
        // Same term: the better placed one stays.
        let p = peers(&[(1, Peer::Main { term: 4 })]);
        assert_eq!(decide(&fo, a, &Duty::Main { term: 4 }, &p, Instant::now()), Verdict::Wait);
        let p = peers(&[(0, Peer::Main { term: 4 })]);
        assert_eq!(decide(&fo, b, &Duty::Main { term: 4 }, &p, Instant::now()), Verdict::Follow(a));
        // A follower moves to the newer main device.
        let p = peers(&[(0, Peer::Main { term: 3 }), (1, Peer::Main { term: 4 })]);
        assert_eq!(decide(&fo, c, &Duty::Following { main: a }, &p, Instant::now()), Verdict::Follow(b));
        assert_eq!(decide(&fo, c, &Duty::Following { main: b }, &p, Instant::now()), Verdict::Wait);
    }

    #[test]
    fn lists_are_kept_in_range() {
        let mut f = list(4);
        f.members.push(f.members[0].clone());
        f.silence = f64::NAN;
        f.clean();
        assert_eq!(f.members.len(), 4);
        assert_eq!(f.silence, 2.0);
        assert!(!list(1).active_for(ids()[0]));
        assert!(list(2).active_for(ids()[1]));
        assert!(!list(2).active_for(ids()[3]));
        let mut no_group = list(2);
        no_group.group = None;
        assert!(!no_group.active_for(ids()[1]));
    }

    fn roster(rev: u64, n: usize) -> Roster {
        Roster { rev, members: list(n).members }
    }

    #[test]
    fn a_newer_list_is_adopted_and_a_removal_is_noticed() {
        let me = ids()[1];
        let mut c = list(3);
        // Older or equal: kept.
        assert_eq!(c.adopt(me, roster(0, 2)), Adopted::Same);
        assert_eq!(c.members.len(), 3);
        assert_eq!(c.adopt(me, roster(1, 3)), Adopted::Same);
        // Newer: taken.
        assert_eq!(c.adopt(me, roster(2, 4)), Adopted::Changed);
        assert_eq!((c.rev, c.members.len()), (2, 4));
        // Newer without us: we were taken out.
        let mut r = roster(3, 4);
        r.members.remove(1);
        assert_eq!(c.adopt(me, r), Adopted::Removed);
        // Two lists at one version: both sides settle on the same one.
        let (mut x, mut y) = (list(3), list(3));
        x.members.swap(0, 1);
        y.members.swap(1, 2);
        let (rx, ry) = (x.roster(), y.roster());
        x.adopt(me, ry);
        y.adopt(me, rx);
        assert_eq!(x.members, y.members);
    }

    #[test]
    fn an_adopted_list_is_cleaned() {
        let me = ids()[0];
        let mut c = list(2);
        let mut r = roster(5, 3);
        r.members.push(r.members[0].clone());
        r.members.extend((0..20).map(|i| Member { id: Uuid::from_u128(100 + i), name: "x\0y".into(), address: "h".repeat(1000) }));
        assert_eq!(c.adopt(me, r), Adopted::Changed);
        assert_eq!(c.members.len(), MAX_MEMBERS);
        assert!(c.members.iter().all(|m| m.address.len() <= 256 && !m.name.contains('\0')));
        let ids: std::collections::HashSet<_> = c.members.iter().map(|m| m.id).collect();
        assert_eq!(ids.len(), c.members.len());
    }

    #[test]
    fn the_group_secret_depends_on_id_and_key_and_stays_out_of_debug() {
        let a = Group::create();
        let b = Group { id: a.id, key: super::super::new_token() };
        let c = Group { id: Uuid::new_v4(), key: a.key.clone() };
        assert_ne!(a.secret(), b.secret());
        assert_ne!(a.secret(), c.secret());
        assert_eq!(a.secret(), a.clone().secret());
        assert!(!format!("{a:?} {:?}", list(2)).contains(&a.key));
    }
}
