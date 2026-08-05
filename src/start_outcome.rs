//! Terminal outcomes for asynchronous session starts, keyed by exact
//! `ResourceOwner`.
//!
//! `/api/sessions/start` returns 202 before any launch work happens, so the
//! HTTP response only acknowledges acceptance. The daemon already knows the
//! terminal result (it logs `async session start complete: ...`); this store
//! makes that result reachable by the caller that asked for the start.
//!
//! Deliberately outside `DaemonState`: these records are ephemeral
//! observability, never persisted, and never participate in session semantics,
//! so they must not enter the pure state machine or its model check. The store
//! uses its own `std::sync::Mutex` with no I/O inside the critical section, so
//! it never shares or extends the protocol lock.
//!
//! Records are keyed by `ResourceOwner` (public session id **plus**
//! incarnation) because both public ids and panes get reused. A caller holding
//! an older incarnation's ticket must never be handed a newer incarnation's
//! result.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::daemon_protocol::ResourceOwner;

/// Maximum retained terminal outcomes. Oldest are dropped first.
pub const MAX_RETAINED_START_OUTCOMES: usize = 256;

/// How long a terminal outcome stays readable. A caller that never polls
/// cannot pin a record beyond this.
pub const START_OUTCOME_TTL: Duration = Duration::from_secs(30 * 60);

/// Terminal disposition of one start attempt for one exact owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartOutcomeStatus {
    /// The launch completed and the session is registered under this owner.
    Started,
    /// The launch terminally failed. The detail carries the logged reason.
    Failed,
    /// A newer incarnation took over before this attempt could finish.
    Superseded,
}

/// What a poller learns about a ticket. Adds the two non-terminal answers the
/// retained records cannot express by themselves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StartStatus {
    Started {
        detail: String,
    },
    Failed {
        detail: String,
    },
    Superseded {
        detail: String,
    },
    /// The launch is still running for this exact owner.
    InProgress,
    /// Nothing is known: the record aged out or the daemon restarted. Not a
    /// success claim.
    Unknown,
}

impl StartStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            StartStatus::Started { .. } => "started",
            StartStatus::Failed { .. } => "failed",
            StartStatus::Superseded { .. } => "superseded",
            StartStatus::InProgress => "in_progress",
            StartStatus::Unknown => "unknown",
        }
    }

    /// Terminal statuses stop a `--wait` poll loop.
    pub fn is_terminal(&self) -> bool {
        !matches!(self, StartStatus::InProgress | StartStatus::Unknown)
    }

    pub fn detail(&self) -> Option<&str> {
        match self {
            StartStatus::Started { detail }
            | StartStatus::Failed { detail }
            | StartStatus::Superseded { detail } => Some(detail.as_str()),
            StartStatus::InProgress | StartStatus::Unknown => None,
        }
    }
}

#[derive(Clone, Debug)]
struct StartOutcomeRecord {
    owner: ResourceOwner,
    status: StartOutcomeStatus,
    detail: String,
    recorded_at: Instant,
}

/// Bounded, TTL-expiring ring of terminal start outcomes.
#[derive(Debug)]
pub struct StartOutcomeStore {
    records: std::sync::Mutex<VecDeque<StartOutcomeRecord>>,
    capacity: usize,
    ttl: Duration,
}

impl Default for StartOutcomeStore {
    fn default() -> Self {
        Self::with_limits(MAX_RETAINED_START_OUTCOMES, START_OUTCOME_TTL)
    }
}

impl StartOutcomeStore {
    pub fn with_limits(capacity: usize, ttl: Duration) -> Self {
        Self {
            records: std::sync::Mutex::new(VecDeque::with_capacity(capacity.min(64))),
            capacity: capacity.max(1),
            ttl,
        }
    }

    /// Record the terminal outcome for one exact owner. A repeat record for
    /// the same owner replaces the earlier one rather than growing the ring.
    pub fn record(&self, owner: &ResourceOwner, status: StartOutcomeStatus, detail: String) {
        let now = Instant::now();
        let mut records = self.lock();
        Self::prune(&mut records, self.ttl, now);
        records.retain(|record| &record.owner != owner);
        records.push_back(StartOutcomeRecord {
            owner: owner.clone(),
            status,
            detail,
            recorded_at: now,
        });
        while records.len() > self.capacity {
            records.pop_front();
        }
    }

    /// Look up the terminal outcome for one exact owner. Never falls back to
    /// another incarnation of the same public id.
    pub fn get(&self, owner: &ResourceOwner) -> Option<StartStatus> {
        let now = Instant::now();
        let mut records = self.lock();
        Self::prune(&mut records, self.ttl, now);
        records
            .iter()
            .rev()
            .find(|record| &record.owner == owner)
            .map(|record| match record.status {
                StartOutcomeStatus::Started => StartStatus::Started {
                    detail: record.detail.clone(),
                },
                StartOutcomeStatus::Failed => StartStatus::Failed {
                    detail: record.detail.clone(),
                },
                StartOutcomeStatus::Superseded => StartStatus::Superseded {
                    detail: record.detail.clone(),
                },
            })
    }

    /// Retained record count after expiry.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        let now = Instant::now();
        let mut records = self.lock();
        Self::prune(&mut records, self.ttl, now);
        records.len()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<StartOutcomeRecord>> {
        self.records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn prune(records: &mut VecDeque<StartOutcomeRecord>, ttl: Duration, now: Instant) {
        while records
            .front()
            .is_some_and(|record| now.duration_since(record.recorded_at) >= ttl)
        {
            records.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon_protocol::SessionIncarnation;

    fn owner(id: &str, incarnation: u64) -> ResourceOwner {
        ResourceOwner {
            session_id: id.to_string(),
            incarnation: SessionIncarnation(incarnation),
        }
    }

    #[test]
    fn a_recorded_success_is_readable_by_its_exact_owner() {
        let store = StartOutcomeStore::default();
        store.record(
            &owner("tp-64", 7),
            StartOutcomeStatus::Started,
            "started 'tp-64' in /tmp/tp-64 (pane %151)".to_string(),
        );
        assert_eq!(
            store.get(&owner("tp-64", 7)),
            Some(StartStatus::Started {
                detail: "started 'tp-64' in /tmp/tp-64 (pane %151)".to_string()
            })
        );
    }

    #[test]
    fn a_failure_reports_the_logged_reason_rather_than_success() {
        let store = StartOutcomeStore::default();
        let reason = "start failed: OpenCode attach setup failed for 'tp-64' (pane %150)";
        store.record(
            &owner("tp-64", 7),
            StartOutcomeStatus::Failed,
            reason.to_string(),
        );
        let status = store.get(&owner("tp-64", 7)).expect("record retained");
        assert_eq!(status.as_str(), "failed");
        assert_eq!(status.detail(), Some(reason));
    }

    #[test]
    fn another_incarnation_never_answers_for_this_owner() {
        let store = StartOutcomeStore::default();
        store.record(
            &owner("tp-64", 8),
            StartOutcomeStatus::Started,
            "started 'tp-64' in /tmp/tp-64 (pane %151)".to_string(),
        );
        // The superseded caller holds incarnation 7 and must not read the
        // newer incarnation's success.
        assert_eq!(store.get(&owner("tp-64", 7)), None);
    }

    #[test]
    fn a_repeat_record_for_one_owner_replaces_rather_than_accumulates() {
        let store = StartOutcomeStore::default();
        store.record(&owner("tp-64", 7), StartOutcomeStatus::Failed, "a".into());
        store.record(&owner("tp-64", 7), StartOutcomeStatus::Started, "b".into());
        assert_eq!(store.len(), 1);
        assert_eq!(
            store.get(&owner("tp-64", 7)),
            Some(StartStatus::Started { detail: "b".into() })
        );
    }

    #[test]
    fn retained_outcome_state_is_bounded_by_capacity() {
        let store = StartOutcomeStore::with_limits(4, START_OUTCOME_TTL);
        for incarnation in 0..64u64 {
            store.record(
                &owner(&format!("s-{incarnation}"), incarnation),
                StartOutcomeStatus::Started,
                "started".into(),
            );
        }
        assert_eq!(store.len(), 4);
        // Oldest are dropped first; the newest four survive.
        assert!(store.get(&owner("s-0", 0)).is_none());
        assert!(store.get(&owner("s-63", 63)).is_some());
    }

    #[test]
    fn a_caller_that_never_polls_does_not_pin_a_record_forever() {
        let store = StartOutcomeStore::with_limits(64, Duration::from_millis(1));
        store.record(&owner("tp-64", 7), StartOutcomeStatus::Started, "ok".into());
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(store.len(), 0);
        assert_eq!(store.get(&owner("tp-64", 7)), None);
    }

    #[test]
    fn only_terminal_statuses_stop_a_wait_loop() {
        assert!(
            StartStatus::Started {
                detail: String::new()
            }
            .is_terminal()
        );
        assert!(
            StartStatus::Failed {
                detail: String::new()
            }
            .is_terminal()
        );
        assert!(
            StartStatus::Superseded {
                detail: String::new()
            }
            .is_terminal()
        );
        assert!(!StartStatus::InProgress.is_terminal());
        assert!(!StartStatus::Unknown.is_terminal());
    }
}
