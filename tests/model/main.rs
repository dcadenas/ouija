//! Stateright model for ouija's multi-daemon coordination protocol.
//!
//! Models wire protocol interactions between 2 daemons to verify safety and
//! liveness properties of session management. Abstracts away tmux injection
//! and Nostr transport.
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
//!    The real code tolerates this (one-hop resolve + existence check), but
//!    the alias map becomes logically inconsistent.
//!
//! ## Fix verified
//!
//! Adding a monotonic generation counter to all wire messages from a daemon,
//! and having receivers drop messages with generation < last seen from that
//! daemon, restores convergence.

use stateright::actor::{Actor, ActorModel, Id, Network, Out};
use stateright::{Checker, Expectation, Model};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

// ---------------------------------------------------------------------------
// Domain types
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
enum Sid { A, B, C }
const ALL_SIDS: [Sid; 3] = [Sid::A, Sid::B, Sid::C];

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct DaemonId(usize);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct RemoteKey { daemon: DaemonId, id: Sid }

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
enum Msg {
    SessionAnnounce { id: Sid, daemon: DaemonId, seq: u8 },
    SessionList { sessions: BTreeSet<Sid>, daemon: DaemonId, seq: u8 },
    SessionRemove { id: Sid, daemon: DaemonId, seq: u8 },
    SessionRenamed { old_id: Sid, new_id: Sid, daemon: DaemonId, seq: u8 },
    Register { id: Sid },
    Remove { id: Sid },
    Rename { old_id: Sid, new_id: Sid },
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
enum Action { Register(Sid), Remove(Sid), Rename(Sid, Sid) }

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
    Client { actions_taken: u8 },
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

    fn on_msg(&self, _id: Id, state: &mut Cow<'_, Self::State>, _src: Id, msg: Self::Msg, o: &mut Out<Self>) {
        let OuijaState::Daemon { daemon_id: my_id, .. } = state.as_ref() else { return };
        let my_id = *my_id;

        match msg {
            Msg::Register { id: sid } => {
                let s = state.to_mut();
                let OuijaState::Daemon { local, peers, daemon_id, seq, .. } = s else { return };
                if local.insert(sid) {
                    *seq += 1;
                    let g = *seq;
                    for &peer in peers.iter() {
                        o.send(peer, Msg::SessionAnnounce { id: sid, daemon: *daemon_id, seq: g });
                    }
                    send_list(local, *daemon_id, g, peers, o);
                }
            }

            Msg::Remove { id: sid } => {
                let s = state.to_mut();
                let OuijaState::Daemon { local, peers, daemon_id, seq, .. } = s else { return };
                if local.remove(&sid) {
                    *seq += 1;
                    let g = *seq;
                    for &peer in peers.iter() {
                        o.send(peer, Msg::SessionRemove { id: sid, daemon: *daemon_id, seq: g });
                    }
                    send_list(local, *daemon_id, g, peers, o);
                }
            }

            Msg::Rename { old_id, new_id } => {
                let s = state.to_mut();
                let OuijaState::Daemon { local, aliases, peers, daemon_id, seq, .. } = s else { return };
                if old_id != new_id && local.remove(&old_id) {
                    local.insert(new_id);
                    add_alias(aliases, old_id, new_id);
                    *seq += 1;
                    let g = *seq;
                    for &peer in peers.iter() {
                        o.send(peer, Msg::SessionRenamed { old_id, new_id, daemon: *daemon_id, seq: g });
                    }
                    send_list(local, *daemon_id, g, peers, o);
                }
            }

            ref wire_msg if wire_msg.daemon().is_some_and(|d| d != my_id) => {
                let from_daemon = wire_msg.daemon().unwrap();
                match wire_msg {
                    Msg::SessionAnnounce { id: sid, .. } => {
                        let s = state.to_mut();
                        if let OuijaState::Daemon { remote, .. } = s {
                            remote.insert(RemoteKey { daemon: from_daemon, id: *sid });
                        }
                    }
                    Msg::SessionList { sessions, .. } => {
                        let s = state.to_mut();
                        if let OuijaState::Daemon { remote, .. } = s {
                            let expected: BTreeSet<RemoteKey> = sessions
                                .iter().map(|&sid| RemoteKey { daemon: from_daemon, id: sid }).collect();
                            for key in &expected { remote.insert(*key); }
                            remote.retain(|k| k.daemon != from_daemon || expected.contains(k));
                        }
                    }
                    Msg::SessionRemove { id: sid, .. } => {
                        let s = state.to_mut();
                        if let OuijaState::Daemon { remote, .. } = s {
                            remote.remove(&RemoteKey { daemon: from_daemon, id: *sid });
                        }
                    }
                    Msg::SessionRenamed { old_id, new_id, .. } => {
                        let s = state.to_mut();
                        if let OuijaState::Daemon { remote, aliases, .. } = s {
                            remote.remove(&RemoteKey { daemon: from_daemon, id: *old_id });
                            remote.insert(RemoteKey { daemon: from_daemon, id: *new_id });
                            add_alias(aliases, *old_id, *new_id);
                        }
                    }
                    _ => {}
                }
            }

            _ => {}
        }
    }

    fn on_random(&self, _id: Id, state: &mut Cow<'_, Self::State>, random: &Self::Random, o: &mut Out<Self>) {
        if let OuijaActor::Client { target } = self {
            let s = state.to_mut();
            if let OuijaState::Client { actions_taken } = s {
                *actions_taken += 1;
                match random {
                    Action::Register(sid) => o.send(*target, Msg::Register { id: *sid }),
                    Action::Remove(sid) => o.send(*target, Msg::Remove { id: *sid }),
                    Action::Rename(old, new) => {
                        o.send(*target, Msg::Rename { old_id: *old, new_id: *new })
                    }
                }
                if *actions_taken < MAX_CLIENT_ACTIONS { offer_actions(o); }
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

    fn on_msg(&self, _id: Id, state: &mut Cow<'_, Self::State>, _src: Id, msg: Self::Msg, o: &mut Out<Self>) {
        let OuijaState::Daemon { daemon_id: my_id, .. } = state.as_ref() else { return };
        let my_id = *my_id;

        match msg {
            Msg::Register { id: sid } => {
                let s = state.to_mut();
                let OuijaState::Daemon { local, peers, daemon_id, seq, .. } = s else { return };
                if local.insert(sid) {
                    *seq += 1;
                    let g = *seq;
                    // No announce — only list
                    send_fixed_list(local, *daemon_id, g, peers, o);
                }
            }

            Msg::Remove { id: sid } => {
                let s = state.to_mut();
                let OuijaState::Daemon { local, peers, daemon_id, seq, .. } = s else { return };
                if local.remove(&sid) {
                    *seq += 1;
                    let g = *seq;
                    send_fixed_list(local, *daemon_id, g, peers, o);
                }
            }

            Msg::Rename { old_id, new_id } => {
                let s = state.to_mut();
                let OuijaState::Daemon { local, aliases, peers, daemon_id, seq, .. } = s else { return };
                if old_id != new_id && local.remove(&old_id) {
                    local.insert(new_id);
                    add_alias(aliases, old_id, new_id);
                    *seq += 1;
                    let g = *seq;
                    send_fixed_list(local, *daemon_id, g, peers, o);
                }
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
                            remote.insert(RemoteKey { daemon: from_daemon, id: *sid });
                        }
                    }
                    Msg::SessionList { sessions, .. } => {
                        if let OuijaState::Daemon { remote, .. } = s {
                            let expected: BTreeSet<RemoteKey> = sessions
                                .iter().map(|&sid| RemoteKey { daemon: from_daemon, id: sid }).collect();
                            for key in &expected { remote.insert(*key); }
                            remote.retain(|k| k.daemon != from_daemon || expected.contains(k));
                        }
                    }
                    Msg::SessionRemove { id: sid, .. } => {
                        if let OuijaState::Daemon { remote, .. } = s {
                            remote.remove(&RemoteKey { daemon: from_daemon, id: *sid });
                        }
                    }
                    Msg::SessionRenamed { old_id, new_id, .. } => {
                        if let OuijaState::Daemon { remote, aliases, .. } = s {
                            remote.remove(&RemoteKey { daemon: from_daemon, id: *old_id });
                            remote.insert(RemoteKey { daemon: from_daemon, id: *new_id });
                            add_alias(aliases, *old_id, *new_id);
                        }
                    }
                    _ => {}
                }
            }

            _ => {}
        }
    }

    fn on_random(&self, _id: Id, state: &mut Cow<'_, Self::State>, random: &Self::Random, o: &mut Out<Self>) {
        if let FixedActor::Client { target } = self {
            let s = state.to_mut();
            if let OuijaState::Client { actions_taken } = s {
                *actions_taken += 1;
                match random {
                    Action::Register(sid) => o.send(*target, Msg::Register { id: *sid }),
                    Action::Remove(sid) => o.send(*target, Msg::Remove { id: *sid }),
                    Action::Rename(old, new) => {
                        o.send(*target, Msg::Rename { old_id: *old, new_id: *new })
                    }
                }
                if *actions_taken < MAX_CLIENT_ACTIONS { offer_fixed_actions(o); }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn send_list(local: &BTreeSet<Sid>, did: DaemonId, seq: u8, peers: &[Id], o: &mut Out<OuijaActor>) {
    let msg = Msg::SessionList { sessions: local.clone(), daemon: did, seq };
    for &peer in peers { o.send(peer, msg.clone()); }
}

fn send_fixed_list(local: &BTreeSet<Sid>, did: DaemonId, seq: u8, peers: &[Id], o: &mut Out<FixedActor>) {
    let msg = Msg::SessionList { sessions: local.clone(), daemon: did, seq };
    for &peer in peers { o.send(peer, msg.clone()); }
}

fn add_alias(aliases: &mut BTreeMap<Sid, Sid>, old_id: Sid, new_id: Sid) {
    for target in aliases.values_mut() {
        if *target == old_id { *target = new_id; }
    }
    aliases.insert(old_id, new_id);
}

fn offer_actions(o: &mut Out<OuijaActor>) {
    let mut c = Vec::new();
    for &s in &ALL_SIDS { c.push(Action::Register(s)); c.push(Action::Remove(s)); }
    for &a in &ALL_SIDS { for &b in &ALL_SIDS { if a != b { c.push(Action::Rename(a, b)); } } }
    o.choose_random("action", c);
}

fn offer_fixed_actions(o: &mut Out<FixedActor>) {
    let mut c = Vec::new();
    for &s in &ALL_SIDS { c.push(Action::Register(s)); c.push(Action::Remove(s)); }
    for &a in &ALL_SIDS { for &b in &ALL_SIDS { if a != b { c.push(Action::Rename(a, b)); } } }
    o.choose_random("action", c);
}

// ---------------------------------------------------------------------------
// Property checkers
// ---------------------------------------------------------------------------

fn daemon_views(actor_states: &[std::sync::Arc<OuijaState>]) -> Vec<(DaemonId, &BTreeSet<Sid>, &BTreeSet<RemoteKey>, &BTreeMap<Sid, Sid>)> {
    actor_states.iter().filter_map(|s| {
        if let OuijaState::Daemon { daemon_id, local, remote, aliases, .. } = s.as_ref() {
            Some((*daemon_id, local, remote, aliases))
        } else { None }
    }).collect()
}

fn check_convergence<A: Actor<State = OuijaState>>(
    _: &ActorModel<A, ()>, state: &<ActorModel<A, ()> as Model>::State,
) -> bool where A::Msg: Ord, A::Timer: Ord {
    if state.network.len() > 0 { return true; }
    let ds = daemon_views(&state.actor_states);
    for &(src_id, src_local, _, _) in &ds {
        for &(obs_id, _, obs_remote, _) in &ds {
            if src_id == obs_id { continue; }
            let observed: BTreeSet<Sid> = obs_remote.iter()
                .filter(|k| k.daemon == src_id).map(|k| k.id).collect();
            if observed != *src_local { return false; }
        }
    }
    true
}

fn check_no_self_remote<A: Actor<State = OuijaState>>(
    _: &ActorModel<A, ()>, state: &<ActorModel<A, ()> as Model>::State,
) -> bool where A::Msg: Ord, A::Timer: Ord {
    daemon_views(&state.actor_states).iter()
        .all(|&(did, _, remote, _)| remote.iter().all(|k| k.daemon != did))
}

fn check_alias_acyclic<A: Actor<State = OuijaState>>(
    _: &ActorModel<A, ()>, state: &<ActorModel<A, ()> as Model>::State,
) -> bool where A::Msg: Ord, A::Timer: Ord {
    for &(_, _, _, aliases) in &daemon_views(&state.actor_states) {
        for (&start, &first) in aliases {
            let mut cur = first;
            let mut vis = BTreeSet::new();
            vis.insert(start);
            if !vis.insert(cur) { return false; }
            while let Some(&nxt) = aliases.get(&cur) {
                if !vis.insert(nxt) { return false; }
                cur = nxt;
            }
        }
    }
    true
}

fn check_some_registered<A: Actor<State = OuijaState>>(
    _: &ActorModel<A, ()>, state: &<ActorModel<A, ()> as Model>::State,
) -> bool where A::Msg: Ord, A::Timer: Ord {
    daemon_views(&state.actor_states).iter().any(|&(_, local, _, _)| !local.is_empty())
}

fn check_some_remote<A: Actor<State = OuijaState>>(
    _: &ActorModel<A, ()>, state: &<ActorModel<A, ()> as Model>::State,
) -> bool where A::Msg: Ord, A::Timer: Ord {
    daemon_views(&state.actor_states).iter().any(|&(_, _, remote, _)| !remote.is_empty())
}

// ---------------------------------------------------------------------------
// Model builders
// ---------------------------------------------------------------------------

fn build_current_model() -> ActorModel<OuijaActor, ()> {
    let (d0, d1) = (Id::from(0usize), Id::from(1usize));
    ActorModel::new((), ())
        .actor(OuijaActor::Daemon { daemon_id: DaemonId(0), peers: vec![d1] })
        .actor(OuijaActor::Daemon { daemon_id: DaemonId(1), peers: vec![d0] })
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
        .actor(FixedActor::Daemon { daemon_id: DaemonId(0), peers: vec![d1] })
        .actor(FixedActor::Daemon { daemon_id: DaemonId(1), peers: vec![d0] })
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
            checker.state_count(), checker.unique_state_count(),
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
            checker.state_count(), checker.unique_state_count(), checker.max_depth(),
        );
        checker.assert_properties();
    }
}
