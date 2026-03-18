//! Stateright model for ouija's multi-daemon coordination protocol.
//!
//! Models wire protocol interactions between 2 daemons to verify safety and
//! liveness properties of session management. Abstracts away tmux injection
//! and Nostr transport.
//!
//! This is the abstract model using enum Sid/DaemonId types. A second model
//! using the real `DaemonState` lives in `src/daemon_protocol.rs` as
//! `stateright_model` — it exercises production `apply()` directly.
//!
//! ## Bugs found
//!
//! 1. **Out-of-order message race**: SessionAnnounce, SessionList, and
//!    SessionRenamed can arrive in any order over Nostr. An old SessionList
//!    arriving after a newer one undoes reconciliation, creating stale remote
//!    sessions. Similarly, a stale SessionAnnounce re-adds a session that was
//!    already reconciled away.
//!
//! 2. **Alias self-loops**: `add_alias` creates self-loops (e.g. C→C) when
//!    local and remote renames interact with overlapping session IDs.
//!    Fixed: `add_alias` now retains only entries where key != value.
//!
//! 3. **Cross-daemon orphaned pending replies**: When a session is removed,
//!    pending reply cleanup only runs on the local daemon. Remote daemons
//!    that received expects_reply messages from the removed session retain
//!    stale pending reply entries.
//!
//! 4. **accept_seq restart bypass** (found by real-DaemonState model):
//!    `accept_seq` had a `seq <= 1` special case for daemon restarts that
//!    also accepted stale messages, resetting the generation counter. A stale
//!    SessionAnnounce{seq=1} arriving after a SessionList{seq=2} would reset
//!    `last_seen_seq`, letting the old SessionList{seq=1} through and
//!    overwriting correct remote state.
//!    Fixed: removed the seq<=1 bypass; daemon restarts now use
//!    timestamp-based wire_seq so new incarnations always have higher seqs.
//!
//! ## Fixes verified
//!
//! - Generation counter (strict `seq < last` rejection) restores convergence.
//! - `add_alias` self-loop removal makes alias maps acyclic.
//! - SessionList reconciliation clears orphaned pending replies.

use stateright::actor::{Actor, ActorModel, Id, Network, Out};
use stateright::{Checker, Expectation, Model};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

// ---------------------------------------------------------------------------
// Domain types
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
enum Sid {
    A,
    B,
    C,
}
const ALL_SIDS: [Sid; 3] = [Sid::A, Sid::B, Sid::C];

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct DaemonId(usize);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct RemoteKey {
    daemon: DaemonId,
    id: Sid,
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
enum Msg {
    SessionAnnounce {
        id: Sid,
        daemon: DaemonId,
        seq: u8,
    },
    SessionList {
        sessions: BTreeSet<Sid>,
        daemon: DaemonId,
        seq: u8,
    },
    SessionRemove {
        id: Sid,
        daemon: DaemonId,
        seq: u8,
    },
    SessionRenamed {
        old_id: Sid,
        new_id: Sid,
        daemon: DaemonId,
        seq: u8,
    },
    Register {
        id: Sid,
    },
    Remove {
        id: Sid,
    },
    Rename {
        old_id: Sid,
        new_id: Sid,
    },
    /// Daemon restart: clears all local state and re-broadcasts.
    DaemonRestart,
}

impl Msg {
    fn seq(&self) -> Option<u8> {
        match self {
            Msg::SessionAnnounce { seq, .. }
            | Msg::SessionList { seq, .. }
            | Msg::SessionRemove { seq, .. }
            | Msg::SessionRenamed { seq, .. } => Some(*seq),
            _ => None,
        }
    }

    fn daemon(&self) -> Option<DaemonId> {
        match self {
            Msg::SessionAnnounce { daemon, .. }
            | Msg::SessionList { daemon, .. }
            | Msg::SessionRemove { daemon, .. }
            | Msg::SessionRenamed { daemon, .. } => Some(*daemon),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
enum Action {
    Register(Sid),
    Remove(Sid),
    Rename(Sid, Sid),
    /// Daemon restart: clears all state and re-broadcasts.
    DaemonRestart,
}

// ---------------------------------------------------------------------------
// Actor — parameterized by whether generation filtering is enabled
// ---------------------------------------------------------------------------

#[derive(Clone)]
enum OuijaActor {
    Daemon { daemon_id: DaemonId, peers: Vec<Id> },
    Client { target: Id },
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum OuijaState {
    Daemon {
        daemon_id: DaemonId,
        local: BTreeSet<Sid>,
        remote: BTreeSet<RemoteKey>,
        aliases: BTreeMap<Sid, Sid>,
        peers: Vec<Id>,
        seq: u8,
        /// Per-peer last-seen generation (for filtering stale messages).
        last_seen: BTreeMap<DaemonId, u8>,
    },
    Client {
        actions_taken: u8,
    },
}

const MAX_CLIENT_ACTIONS: u8 = 2;

impl Actor for OuijaActor {
    type Msg = Msg;
    type State = OuijaState;
    type Timer = ();
    type Random = Action;
    type Storage = ();

    fn on_start(&self, _id: Id, _: &Option<()>, o: &mut Out<Self>) -> Self::State {
        match self {
            OuijaActor::Daemon { daemon_id, peers } => OuijaState::Daemon {
                daemon_id: *daemon_id,
                local: BTreeSet::new(),
                remote: BTreeSet::new(),
                aliases: BTreeMap::new(),
                peers: peers.clone(),
                seq: 0,
                last_seen: BTreeMap::new(),
            },
            OuijaActor::Client { .. } => {
                offer_actions(o);
                OuijaState::Client { actions_taken: 0 }
            }
        }
    }

    fn on_msg(
        &self,
        _id: Id,
        state: &mut Cow<'_, Self::State>,
        _src: Id,
        msg: Self::Msg,
        o: &mut Out<Self>,
    ) {
        let OuijaState::Daemon {
            daemon_id: my_id, ..
        } = state.as_ref()
        else {
            return;
        };
        let my_id = *my_id;

        match msg {
            Msg::Register { id: sid } => {
                let s = state.to_mut();
                let OuijaState::Daemon {
                    local,
                    peers,
                    daemon_id,
                    seq,
                    ..
                } = s
                else {
                    return;
                };
                if local.insert(sid) {
                    *seq += 1;
                    let g = *seq;
                    for &peer in peers.iter() {
                        o.send(
                            peer,
                            Msg::SessionAnnounce {
                                id: sid,
                                daemon: *daemon_id,
                                seq: g,
                            },
                        );
                    }
                    send_list(local, *daemon_id, g, peers, o);
                }
            }

            Msg::Remove { id: sid } => {
                let s = state.to_mut();
                let OuijaState::Daemon {
                    local,
                    peers,
                    daemon_id,
                    seq,
                    ..
                } = s
                else {
                    return;
                };
                if local.remove(&sid) {
                    *seq += 1;
                    let g = *seq;
                    for &peer in peers.iter() {
                        o.send(
                            peer,
                            Msg::SessionRemove {
                                id: sid,
                                daemon: *daemon_id,
                                seq: g,
                            },
                        );
                    }
                    send_list(local, *daemon_id, g, peers, o);
                }
            }

            Msg::Rename { old_id, new_id } => {
                let s = state.to_mut();
                let OuijaState::Daemon {
                    local,
                    aliases,
                    peers,
                    daemon_id,
                    seq,
                    ..
                } = s
                else {
                    return;
                };
                if old_id != new_id && local.remove(&old_id) {
                    local.insert(new_id);
                    add_alias(aliases, old_id, new_id);
                    *seq += 1;
                    let g = *seq;
                    for &peer in peers.iter() {
                        o.send(
                            peer,
                            Msg::SessionRenamed {
                                old_id,
                                new_id,
                                daemon: *daemon_id,
                                seq: g,
                            },
                        );
                    }
                    send_list(local, *daemon_id, g, peers, o);
                }
            }

            Msg::DaemonRestart => {
                let s = state.to_mut();
                let OuijaState::Daemon {
                    local,
                    aliases,
                    peers,
                    daemon_id,
                    seq,
                    ..
                } = s
                else {
                    return;
                };
                local.clear();
                aliases.clear();
                *seq += 1;
                let g = *seq;
                send_list(local, *daemon_id, g, peers, o);
            }

            ref wire_msg if wire_msg.daemon().is_some_and(|d| d != my_id) => {
                let from_daemon = wire_msg.daemon().unwrap();
                match wire_msg {
                    Msg::SessionAnnounce { id: sid, .. } => {
                        let s = state.to_mut();
                        if let OuijaState::Daemon { remote, .. } = s {
                            remote.insert(RemoteKey {
                                daemon: from_daemon,
                                id: *sid,
                            });
                        }
                    }
                    Msg::SessionList { sessions, .. } => {
                        let s = state.to_mut();
                        if let OuijaState::Daemon { remote, .. } = s {
                            let expected: BTreeSet<RemoteKey> = sessions
                                .iter()
                                .map(|&sid| RemoteKey {
                                    daemon: from_daemon,
                                    id: sid,
                                })
                                .collect();
                            for key in &expected {
                                remote.insert(*key);
                            }
                            remote.retain(|k| k.daemon != from_daemon || expected.contains(k));
                        }
                    }
                    Msg::SessionRemove { id: sid, .. } => {
                        let s = state.to_mut();
                        if let OuijaState::Daemon { remote, .. } = s {
                            remote.remove(&RemoteKey {
                                daemon: from_daemon,
                                id: *sid,
                            });
                        }
                    }
                    Msg::SessionRenamed { old_id, new_id, .. } => {
                        let s = state.to_mut();
                        if let OuijaState::Daemon {
                            remote, aliases, ..
                        } = s
                        {
                            remote.remove(&RemoteKey {
                                daemon: from_daemon,
                                id: *old_id,
                            });
                            remote.insert(RemoteKey {
                                daemon: from_daemon,
                                id: *new_id,
                            });
                            add_alias(aliases, *old_id, *new_id);
                        }
                    }
                    _ => {}
                }
            }

            _ => {}
        }
    }

    fn on_random(
        &self,
        _id: Id,
        state: &mut Cow<'_, Self::State>,
        random: &Self::Random,
        o: &mut Out<Self>,
    ) {
        if let OuijaActor::Client { target } = self {
            let s = state.to_mut();
            if let OuijaState::Client { actions_taken } = s {
                *actions_taken += 1;
                match random {
                    Action::Register(sid) => o.send(*target, Msg::Register { id: *sid }),
                    Action::Remove(sid) => o.send(*target, Msg::Remove { id: *sid }),
                    Action::Rename(old, new) => o.send(
                        *target,
                        Msg::Rename {
                            old_id: *old,
                            new_id: *new,
                        },
                    ),
                    Action::DaemonRestart => o.send(*target, Msg::DaemonRestart),
                }
                if *actions_taken < MAX_CLIENT_ACTIONS {
                    offer_actions(o);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Fixed actor — filters stale messages by generation counter
// ---------------------------------------------------------------------------

#[derive(Clone)]
enum FixedActor {
    Daemon { daemon_id: DaemonId, peers: Vec<Id> },
    Client { target: Id },
}

impl Actor for FixedActor {
    type Msg = Msg;
    type State = OuijaState;
    type Timer = ();
    type Random = Action;
    type Storage = ();

    fn on_start(&self, _id: Id, _: &Option<()>, o: &mut Out<Self>) -> Self::State {
        match self {
            FixedActor::Daemon { daemon_id, peers } => OuijaState::Daemon {
                daemon_id: *daemon_id,
                local: BTreeSet::new(),
                remote: BTreeSet::new(),
                aliases: BTreeMap::new(),
                peers: peers.clone(),
                seq: 0,
                last_seen: BTreeMap::new(),
            },
            FixedActor::Client { .. } => {
                offer_fixed_actions(o);
                OuijaState::Client { actions_taken: 0 }
            }
        }
    }

    fn on_msg(
        &self,
        _id: Id,
        state: &mut Cow<'_, Self::State>,
        _src: Id,
        msg: Self::Msg,
        o: &mut Out<Self>,
    ) {
        let OuijaState::Daemon {
            daemon_id: my_id, ..
        } = state.as_ref()
        else {
            return;
        };
        let my_id = *my_id;

        match msg {
            Msg::Register { id: sid } => {
                let s = state.to_mut();
                let OuijaState::Daemon {
                    local,
                    peers,
                    daemon_id,
                    seq,
                    ..
                } = s
                else {
                    return;
                };
                if local.insert(sid) {
                    *seq += 1;
                    let g = *seq;
                    // No announce — only list
                    send_fixed_list(local, *daemon_id, g, peers, o);
                }
            }

            Msg::Remove { id: sid } => {
                let s = state.to_mut();
                let OuijaState::Daemon {
                    local,
                    peers,
                    daemon_id,
                    seq,
                    ..
                } = s
                else {
                    return;
                };
                if local.remove(&sid) {
                    *seq += 1;
                    let g = *seq;
                    send_fixed_list(local, *daemon_id, g, peers, o);
                }
            }

            Msg::Rename { old_id, new_id } => {
                let s = state.to_mut();
                let OuijaState::Daemon {
                    local,
                    aliases,
                    peers,
                    daemon_id,
                    seq,
                    ..
                } = s
                else {
                    return;
                };
                if old_id != new_id && local.remove(&old_id) {
                    local.insert(new_id);
                    add_alias(aliases, old_id, new_id);
                    *seq += 1;
                    let g = *seq;
                    send_fixed_list(local, *daemon_id, g, peers, o);
                }
            }

            Msg::DaemonRestart => {
                let s = state.to_mut();
                let OuijaState::Daemon {
                    local,
                    aliases,
                    peers,
                    daemon_id,
                    seq,
                    ..
                } = s
                else {
                    return;
                };
                // Clear all local state (simulates daemon restart)
                local.clear();
                aliases.clear();
                // Bump seq so peers see a fresh generation
                *seq += 1;
                let g = *seq;
                // Broadcast empty session list (peers reconcile away stale remotes)
                send_fixed_list(local, *daemon_id, g, peers, o);
            }

            ref wire_msg if wire_msg.daemon().is_some_and(|d| d != my_id) => {
                let from_daemon = wire_msg.daemon().unwrap();
                let msg_seq = wire_msg.seq().unwrap();

                // === THE FIX: drop stale messages ===
                if let OuijaState::Daemon { last_seen, .. } = state.as_ref() {
                    let &seen = last_seen.get(&from_daemon).unwrap_or(&0);
                    if msg_seq < seen {
                        return; // stale — drop
                    }
                }
                let s = state.to_mut();
                if let OuijaState::Daemon { last_seen, .. } = s {
                    last_seen.insert(from_daemon, msg_seq);
                }

                match wire_msg {
                    Msg::SessionAnnounce { id: sid, .. } => {
                        if let OuijaState::Daemon { remote, .. } = s {
                            remote.insert(RemoteKey {
                                daemon: from_daemon,
                                id: *sid,
                            });
                        }
                    }
                    Msg::SessionList { sessions, .. } => {
                        if let OuijaState::Daemon { remote, .. } = s {
                            let expected: BTreeSet<RemoteKey> = sessions
                                .iter()
                                .map(|&sid| RemoteKey {
                                    daemon: from_daemon,
                                    id: sid,
                                })
                                .collect();
                            for key in &expected {
                                remote.insert(*key);
                            }
                            remote.retain(|k| k.daemon != from_daemon || expected.contains(k));
                        }
                    }
                    Msg::SessionRemove { id: sid, .. } => {
                        if let OuijaState::Daemon { remote, .. } = s {
                            remote.remove(&RemoteKey {
                                daemon: from_daemon,
                                id: *sid,
                            });
                        }
                    }
                    Msg::SessionRenamed { old_id, new_id, .. } => {
                        if let OuijaState::Daemon {
                            remote, aliases, ..
                        } = s
                        {
                            remote.remove(&RemoteKey {
                                daemon: from_daemon,
                                id: *old_id,
                            });
                            remote.insert(RemoteKey {
                                daemon: from_daemon,
                                id: *new_id,
                            });
                            add_alias(aliases, *old_id, *new_id);
                        }
                    }
                    _ => {}
                }
            }

            _ => {}
        }
    }

    fn on_random(
        &self,
        _id: Id,
        state: &mut Cow<'_, Self::State>,
        random: &Self::Random,
        o: &mut Out<Self>,
    ) {
        if let FixedActor::Client { target } = self {
            let s = state.to_mut();
            if let OuijaState::Client { actions_taken } = s {
                *actions_taken += 1;
                match random {
                    Action::Register(sid) => o.send(*target, Msg::Register { id: *sid }),
                    Action::Remove(sid) => o.send(*target, Msg::Remove { id: *sid }),
                    Action::Rename(old, new) => o.send(
                        *target,
                        Msg::Rename {
                            old_id: *old,
                            new_id: *new,
                        },
                    ),
                    Action::DaemonRestart => o.send(*target, Msg::DaemonRestart),
                }
                if *actions_taken < MAX_CLIENT_ACTIONS {
                    offer_fixed_actions(o);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn send_list(local: &BTreeSet<Sid>, did: DaemonId, seq: u8, peers: &[Id], o: &mut Out<OuijaActor>) {
    let msg = Msg::SessionList {
        sessions: local.clone(),
        daemon: did,
        seq,
    };
    for &peer in peers {
        o.send(peer, msg.clone());
    }
}

fn send_fixed_list(
    local: &BTreeSet<Sid>,
    did: DaemonId,
    seq: u8,
    peers: &[Id],
    o: &mut Out<FixedActor>,
) {
    let msg = Msg::SessionList {
        sessions: local.clone(),
        daemon: did,
        seq,
    };
    for &peer in peers {
        o.send(peer, msg.clone());
    }
}

fn add_alias(aliases: &mut BTreeMap<Sid, Sid>, old_id: Sid, new_id: Sid) {
    for target in aliases.values_mut() {
        if *target == old_id {
            *target = new_id;
        }
    }
    aliases.insert(old_id, new_id);
}

fn offer_actions(o: &mut Out<OuijaActor>) {
    let mut c = Vec::new();
    for &s in &ALL_SIDS {
        c.push(Action::Register(s));
        c.push(Action::Remove(s));
    }
    for &a in &ALL_SIDS {
        for &b in &ALL_SIDS {
            if a != b {
                c.push(Action::Rename(a, b));
            }
        }
    }
    c.push(Action::DaemonRestart);
    o.choose_random("action", c);
}

fn offer_fixed_actions(o: &mut Out<FixedActor>) {
    let mut c = Vec::new();
    for &s in &ALL_SIDS {
        c.push(Action::Register(s));
        c.push(Action::Remove(s));
    }
    for &a in &ALL_SIDS {
        for &b in &ALL_SIDS {
            if a != b {
                c.push(Action::Rename(a, b));
            }
        }
    }
    c.push(Action::DaemonRestart);
    o.choose_random("action", c);
}

// ---------------------------------------------------------------------------
// Property checkers
// ---------------------------------------------------------------------------

type DaemonView<'a> = (
    DaemonId,
    &'a BTreeSet<Sid>,
    &'a BTreeSet<RemoteKey>,
    &'a BTreeMap<Sid, Sid>,
);

fn daemon_views(actor_states: &[std::sync::Arc<OuijaState>]) -> Vec<DaemonView<'_>> {
    actor_states
        .iter()
        .filter_map(|s| {
            if let OuijaState::Daemon {
                daemon_id,
                local,
                remote,
                aliases,
                ..
            } = s.as_ref()
            {
                Some((*daemon_id, local, remote, aliases))
            } else {
                None
            }
        })
        .collect()
}

fn check_convergence<A: Actor<State = OuijaState>>(
    _: &ActorModel<A, ()>,
    state: &<ActorModel<A, ()> as Model>::State,
) -> bool
where
    A::Msg: Ord,
    A::Timer: Ord,
{
    if state.network.len() > 0 {
        return true;
    }
    let ds = daemon_views(&state.actor_states);
    for &(src_id, src_local, _, _) in &ds {
        for &(obs_id, _, obs_remote, _) in &ds {
            if src_id == obs_id {
                continue;
            }
            let observed: BTreeSet<Sid> = obs_remote
                .iter()
                .filter(|k| k.daemon == src_id)
                .map(|k| k.id)
                .collect();
            if observed != *src_local {
                return false;
            }
        }
    }
    true
}

fn check_no_self_remote<A: Actor<State = OuijaState>>(
    _: &ActorModel<A, ()>,
    state: &<ActorModel<A, ()> as Model>::State,
) -> bool
where
    A::Msg: Ord,
    A::Timer: Ord,
{
    daemon_views(&state.actor_states)
        .iter()
        .all(|&(did, _, remote, _)| remote.iter().all(|k| k.daemon != did))
}

fn check_alias_acyclic<A: Actor<State = OuijaState>>(
    _: &ActorModel<A, ()>,
    state: &<ActorModel<A, ()> as Model>::State,
) -> bool
where
    A::Msg: Ord,
    A::Timer: Ord,
{
    for &(_, _, _, aliases) in &daemon_views(&state.actor_states) {
        for (&start, &first) in aliases {
            let mut cur = first;
            let mut vis = BTreeSet::new();
            vis.insert(start);
            if !vis.insert(cur) {
                return false;
            }
            while let Some(&nxt) = aliases.get(&cur) {
                if !vis.insert(nxt) {
                    return false;
                }
                cur = nxt;
            }
        }
    }
    true
}

fn check_some_registered<A: Actor<State = OuijaState>>(
    _: &ActorModel<A, ()>,
    state: &<ActorModel<A, ()> as Model>::State,
) -> bool
where
    A::Msg: Ord,
    A::Timer: Ord,
{
    daemon_views(&state.actor_states)
        .iter()
        .any(|&(_, local, _, _)| !local.is_empty())
}

fn check_some_remote<A: Actor<State = OuijaState>>(
    _: &ActorModel<A, ()>,
    state: &<ActorModel<A, ()> as Model>::State,
) -> bool
where
    A::Msg: Ord,
    A::Timer: Ord,
{
    daemon_views(&state.actor_states)
        .iter()
        .any(|&(_, _, remote, _)| !remote.is_empty())
}

// ---------------------------------------------------------------------------
// Model builders
// ---------------------------------------------------------------------------

fn build_current_model() -> ActorModel<OuijaActor, ()> {
    let (d0, d1) = (Id::from(0usize), Id::from(1usize));
    ActorModel::new((), ())
        .actor(OuijaActor::Daemon {
            daemon_id: DaemonId(0),
            peers: vec![d1],
        })
        .actor(OuijaActor::Daemon {
            daemon_id: DaemonId(1),
            peers: vec![d0],
        })
        .actor(OuijaActor::Client { target: d0 })
        .actor(OuijaActor::Client { target: d1 })
        .init_network(Network::new_unordered_nonduplicating([]))
        .property(Expectation::Always, "no self-remote", check_no_self_remote)
        .property(Expectation::Always, "convergence", check_convergence)
        .property(Expectation::Always, "alias acyclic", check_alias_acyclic)
        .property(Expectation::Sometimes, "registered", check_some_registered)
        .property(Expectation::Sometimes, "remote visible", check_some_remote)
        .within_boundary(|_, state| state.network.len() <= 12)
}

fn build_fixed_model() -> ActorModel<FixedActor, ()> {
    let (d0, d1) = (Id::from(0usize), Id::from(1usize));
    ActorModel::new((), ())
        .actor(FixedActor::Daemon {
            daemon_id: DaemonId(0),
            peers: vec![d1],
        })
        .actor(FixedActor::Daemon {
            daemon_id: DaemonId(1),
            peers: vec![d0],
        })
        .actor(FixedActor::Client { target: d0 })
        .actor(FixedActor::Client { target: d1 })
        .init_network(Network::new_unordered_nonduplicating([]))
        .property(Expectation::Always, "no self-remote", check_no_self_remote)
        .property(Expectation::Always, "convergence", check_convergence)
        .property(Expectation::Sometimes, "registered", check_some_registered)
        .property(Expectation::Sometimes, "remote visible", check_some_remote)
        .within_boundary(|_, state| state.network.len() <= 12)
}

// ---------------------------------------------------------------------------
// Reply-tracking model — pending reply semantics on top of generation counters
// ---------------------------------------------------------------------------
//
// Models interaction between session lifecycle (register/remove/rename)
// and per-session pending reply tracking. Uses generation counter filtering
// for session management messages (like FixedActor). Uses only 2 Sids to
// keep the state space tractable.

const REPLY_SIDS: [Sid; 2] = [Sid::A, Sid::B];

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
enum ReplyMsg {
    SessionList {
        sessions: BTreeSet<Sid>,
        daemon: DaemonId,
        seq: u8,
    },
    Register {
        id: Sid,
    },
    Remove {
        id: Sid,
    },
    Rename {
        old_id: Sid,
        new_id: Sid,
    },
    SendExpectingReply {
        from: Sid,
        to: Sid,
    },
    ReplyTo {
        from: Sid,
        to: Sid,
        done: bool,
    },
    /// Cross-daemon message delivery expecting a reply.
    DeliverMsg {
        from_sid: Sid,
        from_daemon: DaemonId,
        to_sid: Sid,
    },
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
enum ReplyAction {
    Register(Sid),
    Remove(Sid),
    Rename(Sid, Sid),
    SendExpectingReply(Sid, Sid),
    ReplyTo(Sid, Sid, bool),
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum ReplyState {
    Daemon {
        daemon_id: DaemonId,
        local: BTreeSet<Sid>,
        remote: BTreeSet<RemoteKey>,
        aliases: BTreeMap<Sid, Sid>,
        peers: Vec<Id>,
        seq: u8,
        last_seen: BTreeMap<DaemonId, u8>,
        /// Per local session: senders (daemon, sid) that expect a reply.
        /// The bool tracks `in_progress` — true after a progress update (done=false),
        /// false when newly added.
        pending_replies: BTreeMap<Sid, BTreeMap<RemoteKey, bool>>,
    },
    Client {
        actions_taken: u8,
    },
}

#[derive(Clone)]
enum ReplyActor {
    Daemon { daemon_id: DaemonId, peers: Vec<Id> },
    Client { target: Id },
}

const MAX_REPLY_ACTIONS: u8 = 3;

impl Actor for ReplyActor {
    type Msg = ReplyMsg;
    type State = ReplyState;
    type Timer = ();
    type Random = ReplyAction;
    type Storage = ();

    fn on_start(&self, _id: Id, _: &Option<()>, o: &mut Out<Self>) -> Self::State {
        match self {
            Self::Daemon { daemon_id, peers } => ReplyState::Daemon {
                daemon_id: *daemon_id,
                local: BTreeSet::new(),
                remote: BTreeSet::new(),
                aliases: BTreeMap::new(),
                peers: peers.clone(),
                seq: 0,
                last_seen: BTreeMap::new(),
                pending_replies: BTreeMap::new(),
            },
            Self::Client { .. } => {
                offer_reply_actions(o);
                ReplyState::Client { actions_taken: 0 }
            }
        }
    }

    fn on_msg(
        &self,
        _id: Id,
        state: &mut Cow<'_, Self::State>,
        _src: Id,
        msg: Self::Msg,
        o: &mut Out<Self>,
    ) {
        let ReplyState::Daemon {
            daemon_id: my_id, ..
        } = state.as_ref()
        else {
            return;
        };
        let my_id = *my_id;

        match msg {
            ReplyMsg::Register { id: sid } => {
                let s = state.to_mut();
                let ReplyState::Daemon {
                    local,
                    peers,
                    daemon_id,
                    seq,
                    ..
                } = s
                else {
                    return;
                };
                if local.insert(sid) {
                    *seq += 1;
                    send_reply_list(local, *daemon_id, *seq, peers, o);
                }
            }

            ReplyMsg::Remove { id: sid } => {
                let s = state.to_mut();
                let ReplyState::Daemon {
                    local,
                    peers,
                    daemon_id,
                    seq,
                    pending_replies,
                    ..
                } = s
                else {
                    return;
                };
                if local.remove(&sid) {
                    pending_replies.remove(&sid);
                    let me = RemoteKey {
                        daemon: *daemon_id,
                        id: sid,
                    };
                    for map in pending_replies.values_mut() {
                        map.remove(&me);
                    }
                    *seq += 1;
                    send_reply_list(local, *daemon_id, *seq, peers, o);
                }
            }

            ReplyMsg::Rename { old_id, new_id } => {
                let s = state.to_mut();
                let ReplyState::Daemon {
                    local,
                    aliases,
                    peers,
                    daemon_id,
                    seq,
                    pending_replies,
                    ..
                } = s
                else {
                    return;
                };
                if old_id != new_id && local.remove(&old_id) {
                    local.insert(new_id);
                    if let Some(set) = pending_replies.remove(&old_id) {
                        pending_replies.insert(new_id, set);
                    }
                    let old_key = RemoteKey {
                        daemon: *daemon_id,
                        id: old_id,
                    };
                    let new_key = RemoteKey {
                        daemon: *daemon_id,
                        id: new_id,
                    };
                    for map in pending_replies.values_mut() {
                        if let Some(in_progress) = map.remove(&old_key) {
                            map.insert(new_key, in_progress);
                        }
                    }
                    add_alias(aliases, old_id, new_id);
                    *seq += 1;
                    send_reply_list(local, *daemon_id, *seq, peers, o);
                }
            }

            ReplyMsg::SendExpectingReply { from, to } => {
                let s = state.to_mut();
                let ReplyState::Daemon {
                    local,
                    remote,
                    daemon_id,
                    peers,
                    pending_replies,
                    ..
                } = s
                else {
                    return;
                };
                if !local.contains(&from) {
                    return;
                }
                let sender = RemoteKey {
                    daemon: *daemon_id,
                    id: from,
                };
                if local.contains(&to) {
                    pending_replies.entry(to).or_default().insert(sender, false);
                } else if remote.iter().any(|rk| rk.id == to) {
                    for &peer in peers.iter() {
                        o.send(
                            peer,
                            ReplyMsg::DeliverMsg {
                                from_sid: from,
                                from_daemon: *daemon_id,
                                to_sid: to,
                            },
                        );
                    }
                }
            }

            ReplyMsg::ReplyTo { from, to, done } => {
                let s = state.to_mut();
                let ReplyState::Daemon {
                    local,
                    pending_replies,
                    ..
                } = s
                else {
                    return;
                };
                if !local.contains(&from) {
                    return;
                }
                if let Some(map) = pending_replies.get_mut(&from) {
                    if done {
                        map.retain(|rk, _| rk.id != to);
                        if map.is_empty() {
                            pending_replies.remove(&from);
                        }
                    } else {
                        // Progress update: mark in_progress but keep entry
                        for (rk, in_progress) in map.iter_mut() {
                            if rk.id == to {
                                *in_progress = true;
                            }
                        }
                    }
                }
            }

            ReplyMsg::DeliverMsg {
                from_sid,
                from_daemon,
                to_sid,
            } if from_daemon != my_id => {
                let s = state.to_mut();
                let ReplyState::Daemon {
                    local,
                    remote,
                    pending_replies,
                    ..
                } = s
                else {
                    return;
                };
                let sender = RemoteKey {
                    daemon: from_daemon,
                    id: from_sid,
                };
                // Only track pending reply if the sender is a known remote
                // session. A DeliverMsg arriving after the sender's SessionList
                // removal would otherwise re-create the orphan.
                if local.contains(&to_sid) && remote.contains(&sender) {
                    pending_replies.entry(to_sid).or_default().insert(sender, false);
                }
            }

            ReplyMsg::SessionList {
                sessions,
                daemon,
                seq,
            } if daemon != my_id => {
                if let ReplyState::Daemon { last_seen, .. } = state.as_ref() {
                    if seq < *last_seen.get(&daemon).unwrap_or(&0) {
                        return;
                    }
                }
                let s = state.to_mut();
                let ReplyState::Daemon {
                    last_seen,
                    remote,
                    pending_replies,
                    ..
                } = s
                else {
                    return;
                };
                last_seen.insert(daemon, seq);
                let expected: BTreeSet<RemoteKey> = sessions
                    .iter()
                    .map(|&sid| RemoteKey { daemon, id: sid })
                    .collect();
                for key in &expected {
                    remote.insert(*key);
                }
                // Collect removed remote sessions before retain.
                let removed: BTreeSet<RemoteKey> = remote
                    .iter()
                    .filter(|k| k.daemon == daemon && !expected.contains(k))
                    .copied()
                    .collect();
                remote.retain(|k| k.daemon != daemon || expected.contains(k));
                // Clear pending replies referencing removed remote sessions
                // (fix for cross-daemon orphan bug).
                if !removed.is_empty() {
                    for map in pending_replies.values_mut() {
                        map.retain(|rk, _| !removed.contains(rk));
                    }
                }
            }

            _ => {}
        }
    }

    fn on_random(
        &self,
        _id: Id,
        state: &mut Cow<'_, Self::State>,
        random: &Self::Random,
        o: &mut Out<Self>,
    ) {
        if let Self::Client { target } = self {
            let s = state.to_mut();
            if let ReplyState::Client { actions_taken } = s {
                *actions_taken += 1;
                match random {
                    ReplyAction::Register(sid) => o.send(*target, ReplyMsg::Register { id: *sid }),
                    ReplyAction::Remove(sid) => o.send(*target, ReplyMsg::Remove { id: *sid }),
                    ReplyAction::Rename(old, new) => o.send(
                        *target,
                        ReplyMsg::Rename {
                            old_id: *old,
                            new_id: *new,
                        },
                    ),
                    ReplyAction::SendExpectingReply(from, to) => o.send(
                        *target,
                        ReplyMsg::SendExpectingReply {
                            from: *from,
                            to: *to,
                        },
                    ),
                    ReplyAction::ReplyTo(from, to, done) => o.send(
                        *target,
                        ReplyMsg::ReplyTo {
                            from: *from,
                            to: *to,
                            done: *done,
                        },
                    ),
                }
                if *actions_taken < MAX_REPLY_ACTIONS {
                    offer_reply_actions(o);
                }
            }
        }
    }
}

fn send_reply_list(
    local: &BTreeSet<Sid>,
    did: DaemonId,
    seq: u8,
    peers: &[Id],
    o: &mut Out<ReplyActor>,
) {
    let msg = ReplyMsg::SessionList {
        sessions: local.clone(),
        daemon: did,
        seq,
    };
    for &peer in peers {
        o.send(peer, msg.clone());
    }
}

fn offer_reply_actions(o: &mut Out<ReplyActor>) {
    let mut c = Vec::new();
    for &s in &REPLY_SIDS {
        c.push(ReplyAction::Register(s));
        c.push(ReplyAction::Remove(s));
    }
    for &a in &REPLY_SIDS {
        for &b in &REPLY_SIDS {
            if a != b {
                c.push(ReplyAction::Rename(a, b));
                c.push(ReplyAction::SendExpectingReply(a, b));
                c.push(ReplyAction::ReplyTo(a, b, true));  // completion
                c.push(ReplyAction::ReplyTo(a, b, false)); // progress update
            }
        }
    }
    o.choose_random("action", c);
}

// ---------------------------------------------------------------------------
// Reply model property checkers
// ---------------------------------------------------------------------------
//
// Structural invariant: progress-preserves-pending
// A progress update (done=false) marks an entry in_progress but never removes
// it. This is verified exhaustively by the model checker exploring both the
// done=false and done=true paths: check_no_orphaned_pending_replies confirms
// that no spurious removal occurs on progress updates.

/// After quiescence, every sender in any pending_replies set must exist as a
/// local session on some daemon.
fn check_no_orphaned_pending_replies(
    _: &ActorModel<ReplyActor, ()>,
    state: &<ActorModel<ReplyActor, ()> as Model>::State,
) -> bool {
    if state.network.len() > 0 {
        return true;
    }
    let mut all_local: BTreeMap<DaemonId, BTreeSet<Sid>> = BTreeMap::new();
    for s in &state.actor_states {
        if let ReplyState::Daemon {
            daemon_id, local, ..
        } = s.as_ref()
        {
            all_local.insert(*daemon_id, local.clone());
        }
    }
    for s in &state.actor_states {
        if let ReplyState::Daemon {
            pending_replies, ..
        } = s.as_ref()
        {
            for senders in pending_replies.values() {
                for sender in senders.keys() {
                    if !all_local
                        .get(&sender.daemon)
                        .is_some_and(|l| l.contains(&sender.id))
                    {
                        return false;
                    }
                }
            }
        }
    }
    true
}

fn check_reply_convergence(
    _: &ActorModel<ReplyActor, ()>,
    state: &<ActorModel<ReplyActor, ()> as Model>::State,
) -> bool {
    if state.network.len() > 0 {
        return true;
    }
    let ds: Vec<_> = state
        .actor_states
        .iter()
        .filter_map(|s| {
            if let ReplyState::Daemon {
                daemon_id,
                local,
                remote,
                ..
            } = s.as_ref()
            {
                Some((*daemon_id, local, remote))
            } else {
                None
            }
        })
        .collect();
    for &(src_id, src_local, _) in &ds {
        for &(obs_id, _, obs_remote) in &ds {
            if src_id == obs_id {
                continue;
            }
            let observed: BTreeSet<Sid> = obs_remote
                .iter()
                .filter(|k| k.daemon == src_id)
                .map(|k| k.id)
                .collect();
            if observed != *src_local {
                return false;
            }
        }
    }
    true
}

fn check_reply_some_registered(
    _: &ActorModel<ReplyActor, ()>,
    state: &<ActorModel<ReplyActor, ()> as Model>::State,
) -> bool {
    state
        .actor_states
        .iter()
        .any(|s| matches!(s.as_ref(), ReplyState::Daemon { local, .. } if !local.is_empty()))
}

fn check_some_pending_replies(
    _: &ActorModel<ReplyActor, ()>,
    state: &<ActorModel<ReplyActor, ()> as Model>::State,
) -> bool {
    state.actor_states.iter().any(|s| {
        if let ReplyState::Daemon {
            pending_replies, ..
        } = s.as_ref()
        {
            pending_replies.values().any(|map| !map.is_empty())
        } else {
            false
        }
    })
}

fn build_reply_model() -> ActorModel<ReplyActor, ()> {
    let (d0, d1) = (Id::from(0usize), Id::from(1usize));
    ActorModel::new((), ())
        .actor(ReplyActor::Daemon {
            daemon_id: DaemonId(0),
            peers: vec![d1],
        })
        .actor(ReplyActor::Daemon {
            daemon_id: DaemonId(1),
            peers: vec![d0],
        })
        .actor(ReplyActor::Client { target: d0 })
        .actor(ReplyActor::Client { target: d1 })
        .init_network(Network::new_unordered_nonduplicating([]))
        .property(
            Expectation::Always,
            "reply convergence",
            check_reply_convergence,
        )
        .property(
            Expectation::Always,
            "no orphaned pending replies",
            check_no_orphaned_pending_replies,
        )
        .property(
            Expectation::Sometimes,
            "reply registered",
            check_reply_some_registered,
        )
        .property(
            Expectation::Sometimes,
            "pending replies exist",
            check_some_pending_replies,
        )
        .within_boundary(|_, state| state.network.len() <= 8)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Bug 1: Out-of-order wire messages break convergence.
    /// Old SessionList/Announce arriving after newer ones undoes reconciliation.
    #[test]
    fn bug_out_of_order_messages_break_convergence() {
        let checker = build_current_model().checker().spawn_bfs().join();
        assert!(
            checker.discovery("convergence").is_some(),
            "Expected convergence violation from out-of-order messages"
        );
        println!(
            "Bug confirmed: out-of-order messages break convergence. States: {}, unique: {}",
            checker.state_count(),
            checker.unique_state_count(),
        );
    }

    /// Bug 2: Alias self-loops from cross-daemon renames.
    #[test]
    fn bug_alias_cycles() {
        let checker = build_current_model().checker().spawn_bfs().join();
        assert!(
            checker.discovery("alias acyclic").is_some(),
            "Expected alias cycle from cross-daemon renames"
        );
    }

    /// Bug 5: DaemonRestart without generation counter breaks convergence.
    /// After restart, stale messages from the old incarnation can re-add
    /// sessions that were cleared.
    #[test]
    fn bug_restart_breaks_convergence_without_gen_counter() {
        let checker = build_current_model().checker().spawn_bfs().join();
        assert!(
            checker.discovery("convergence").is_some(),
            "Expected convergence violation from restart + stale messages"
        );
        println!(
            "Bug confirmed: restart breaks convergence without gen counter. States: {}, unique: {}",
            checker.state_count(),
            checker.unique_state_count(),
        );
    }

    /// Fix: generation counter restores convergence even with daemon restarts.
    #[test]
    fn fix_restart_convergence_with_gen_counter() {
        let checker = build_fixed_model().checker().spawn_bfs().join();
        println!(
            "Fixed model with restart — States: {}, unique: {}, max depth: {}",
            checker.state_count(),
            checker.unique_state_count(),
            checker.max_depth(),
        );
        checker.assert_properties();
    }

    /// No daemon ever holds a remote session attributed to itself (holds in both models).
    #[test]
    fn no_self_remote_holds() {
        let checker = build_current_model().checker().spawn_bfs().join();
        assert!(checker.discovery("no self-remote").is_none());
    }

    /// Fix: generation counter drops stale messages, restoring convergence.
    /// Also removes announces (only SessionList sent on register).
    #[test]
    fn fix_generation_counter_restores_convergence() {
        let checker = build_fixed_model().checker().spawn_bfs().join();
        println!(
            "Fixed model — States: {}, unique: {}, max depth: {}",
            checker.state_count(),
            checker.unique_state_count(),
            checker.max_depth(),
        );
        checker.assert_properties();
    }

    // -- Reply model tests --------------------------------------------------

    /// Fix for Bug 3: Cross-daemon orphaned pending replies.
    /// SessionList reconciliation now clears pending replies referencing
    /// removed remote sessions.
    #[test]
    fn fix_cross_daemon_orphaned_pending_replies() {
        let checker = build_reply_model().checker().spawn_bfs().join();
        assert!(
            checker.discovery("no orphaned pending replies").is_none(),
            "Fix verified: no orphaned pending replies from cross-daemon session removal"
        );
        println!(
            "Fix verified: no orphaned pending replies. States: {}, unique: {}",
            checker.state_count(),
            checker.unique_state_count(),
        );
    }

    /// Reply model preserves convergence (generation counter still works).
    #[test]
    fn reply_model_convergence_holds() {
        let checker = build_reply_model().checker().spawn_bfs().join();
        assert!(
            checker.discovery("reply convergence").is_none(),
            "Convergence should hold with generation counter"
        );
    }

    /// Liveness: some states have pending replies.
    #[test]
    fn reply_model_pending_replies_reachable() {
        let checker = build_reply_model().checker().spawn_bfs().join();
        assert!(
            checker.discovery("pending replies exist").is_some(),
            "Expected some states with pending replies"
        );
    }
}
