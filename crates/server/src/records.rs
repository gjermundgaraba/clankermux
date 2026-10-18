//! Program status records (OSC 7501), one per record id.

use std::collections::BTreeMap;

use ghostty_vt::{ProgramStatus, ProgramStatusKind, ProgramStatusState};
use proto::StatusRecord;

/// The records of one session. Each carries the tick of its last report;
/// the lowest is evicted first.
#[derive(Default)]
pub struct Records {
    by_id: BTreeMap<String, (StatusRecord, u64)>,
    tick: u64,
}

/// What a report changed: a record set, and the ids of records dropped.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Change {
    pub set: Option<StatusRecord>,
    pub removed: Vec<String>,
}

/// Whether a record's state is one the program is still in the middle
/// of: dropped at a prompt start, at the end of the stream, and on load.
fn transient(record: &StatusRecord) -> bool {
    matches!(record.state.as_str(), "working" | "blocked")
}

fn text(value: String) -> Option<String> {
    (!value.is_empty()).then_some(value)
}

impl Records {
    /// Records loaded from a park file, minus the transient ones. A
    /// restored session is never live again, so their order (which only
    /// eviction reads) does not matter.
    pub fn load(records: Vec<StatusRecord>) -> Self {
        let mut out = Records::default();
        for record in records.into_iter().filter(|r| !transient(r)) {
            out.insert(record);
        }
        out
    }

    fn insert(&mut self, record: StatusRecord) {
        self.tick += 1;
        self.by_id.insert(record.id.clone(), (record, self.tick));
    }

    /// Applies one report: `clear` removes the record and those beneath
    /// it; any other state replaces the record, holding exactly the keys
    /// of this report. Beyond `max` records the least recently updated
    /// one goes.
    pub fn apply(&mut self, report: ProgramStatus, max: usize, now: &str) -> Change {
        let mut change = Change::default();
        if report.state == ProgramStatusState::Clear {
            let beneath = |id: &str| {
                report.id.is_empty()
                    || id == report.id
                    || id.starts_with(&format!("{}/", report.id))
            };
            change.removed = self
                .by_id
                .keys()
                .filter(|id| beneath(id))
                .cloned()
                .collect();
            for id in &change.removed {
                self.by_id.remove(id);
            }
            return change;
        }
        let state = match report.state {
            ProgramStatusState::Idle => "idle",
            ProgramStatusState::Working => "working",
            ProgramStatusState::Done => "done",
            ProgramStatusState::Blocked => "blocked",
            ProgramStatusState::Error => "error",
            ProgramStatusState::Clear => unreachable!(),
        };
        // The engine clears `kind` outside `blocked` and `progress` outside
        // `working` and `blocked` or 0..=100 (terminal.h).
        let kind = match report.kind {
            ProgramStatusKind::None => None,
            ProgramStatusKind::Permission => Some("permission"),
            ProgramStatusKind::Question => Some("question"),
            ProgramStatusKind::Auth => Some("auth"),
        };
        let record = StatusRecord {
            id: report.id,
            state: state.into(),
            kind: kind.map(String::from),
            app: text(report.app),
            title: text(report.title),
            msg: text(report.message),
            progress: u8::try_from(report.progress).ok(),
            updated_at: now.into(),
        };
        let new = !self.by_id.contains_key(&record.id);
        self.insert(record.clone());
        change.set = Some(record);
        if new && self.by_id.len() > max {
            let oldest = self
                .by_id
                .iter()
                .min_by_key(|(_, (_, tick))| *tick)
                .map(|(id, _)| id.clone())
                .expect("over max means at least one record");
            self.by_id.remove(&oldest);
            change.removed.push(oldest);
        }
        change
    }

    /// Drops the `working` and `blocked` records; returns their ids.
    pub fn drop_transient(&mut self) -> Vec<String> {
        let ids: Vec<String> = self
            .by_id
            .values()
            .filter(|(r, _)| transient(r))
            .map(|(r, _)| r.id.clone())
            .collect();
        for id in &ids {
            self.by_id.remove(id);
        }
        ids
    }

    /// The records as reported: every key of their last report.
    pub fn all(&self) -> Vec<StatusRecord> {
        self.by_id.values().map(|(r, _)| r.clone()).collect()
    }

    /// The records for `inspect`: `app` taken from the nearest ancestor
    /// that has one when a record lacks it.
    pub fn resolved(&self) -> Vec<StatusRecord> {
        let app_of = |id: &str| self.by_id.get(id).and_then(|(r, _)| r.app.clone());
        self.by_id
            .values()
            .map(|(record, _)| {
                let mut out = record.clone();
                if out.app.is_none() {
                    let mut id = record.id.as_str();
                    while let Some(cut) = id.rfind('/') {
                        id = &id[..cut];
                        if let Some(app) = app_of(id) {
                            out.app = Some(app);
                            break;
                        }
                    }
                    if out.app.is_none() && !record.id.is_empty() {
                        out.app = app_of("");
                    }
                }
                out
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(state: ProgramStatusState, id: &str, app: &str) -> ProgramStatus {
        ProgramStatus {
            state,
            kind: ProgramStatusKind::None,
            progress: -1,
            id: id.into(),
            app: app.into(),
            title: String::new(),
            message: String::new(),
        }
    }

    fn record(id: &str, state: &str, updated_at: &str) -> StatusRecord {
        StatusRecord {
            id: id.into(),
            state: state.into(),
            kind: None,
            app: None,
            title: None,
            msg: None,
            progress: None,
            updated_at: updated_at.into(),
        }
    }

    #[test]
    fn reports_replace_clear_and_evict() {
        let mut r = Records::default();
        let c = r.apply(report(ProgramStatusState::Working, "", "cargo"), 2, "t1");
        assert_eq!(c.set.as_ref().map(|s| s.state.as_str()), Some("working"));
        assert_eq!(c.set.as_ref().unwrap().app.as_deref(), Some("cargo"));
        assert!(c.set.as_ref().unwrap().progress.is_none());
        r.apply(report(ProgramStatusState::Done, "deploy", ""), 2, "t2");
        // A third record evicts the least recently updated: the root.
        let c = r.apply(report(ProgramStatusState::Idle, "deploy/us", ""), 2, "t3");
        assert_eq!(c.removed, vec![String::new()]);
        assert_eq!(r.all().len(), 2);
        // Replacing an existing record does not evict.
        let c = r.apply(report(ProgramStatusState::Done, "deploy", "tf"), 2, "t4");
        assert!(c.removed.is_empty());
        // The child takes its app from the nearest ancestor at inspect.
        let resolved = r.resolved();
        assert_eq!(
            resolved
                .iter()
                .find(|x| x.id == "deploy/us")
                .unwrap()
                .app
                .as_deref(),
            Some("tf")
        );
        assert!(
            r.all()
                .iter()
                .find(|x| x.id == "deploy/us")
                .unwrap()
                .app
                .is_none()
        );
        // Clearing a parent drops its subtree.
        let c = r.apply(report(ProgramStatusState::Clear, "deploy", ""), 2, "t5");
        assert_eq!(
            c.removed,
            vec!["deploy".to_string(), "deploy/us".to_string()]
        );
        assert!(r.all().is_empty());
    }

    #[test]
    fn transient_records_are_dropped_and_blocked_keeps_its_kind() {
        let mut r = Records::default();
        let mut blocked = report(ProgramStatusState::Blocked, "", "");
        blocked.kind = ProgramStatusKind::Permission;
        blocked.progress = 40;
        let c = r.apply(blocked, 10, "t");
        let set = c.set.unwrap();
        assert_eq!(
            (set.kind.as_deref(), set.progress),
            (Some("permission"), Some(40))
        );
        r.apply(report(ProgramStatusState::Done, "x", ""), 10, "t");
        assert_eq!(r.drop_transient(), vec![String::new()]);
        assert_eq!(r.all().len(), 1);
        let c = r.apply(report(ProgramStatusState::Clear, "", ""), 10, "t");
        assert_eq!(c.removed, vec!["x".to_string()]);
    }

    #[test]
    fn loading_drops_transient_records() {
        let loaded = Records::load(vec![
            record("c", "done", "t1"),
            record("b", "error", "t2"),
            record("a", "working", "t3"),
        ]);
        let mut ids: Vec<_> = loaded.all().into_iter().map(|x| x.id).collect();
        ids.sort();
        assert_eq!(ids, vec!["b".to_string(), "c".to_string()]);
    }
}
