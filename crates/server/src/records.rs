//! Program status records (OSC 7501), one per record id.

use std::collections::BTreeMap;

use ghostty_vt::{ProgramStatus, ProgramStatusKind, ProgramStatusState};
use proto::{Attention, StatusRecord};

/// The records of one session. Each carries the tick of its last report;
/// the lowest is evicted first.
#[derive(Default)]
pub struct Records {
    by_id: BTreeMap<String, (StatusRecord, u64)>,
    tick: u64,
}

/// What a report changed: a record set, and the ids of records dropped,
/// by a clear when none was set and by eviction when one was.
#[derive(Default)]
pub struct Change {
    pub set: Option<StatusRecord>,
    pub removed: Vec<String>,
}

/// Whether a record's state is one the program is still in the middle
/// of: dropped at a prompt start, at the end of the stream, and on load.
fn transient(record: &StatusRecord) -> bool {
    matches!(record.state.as_str(), "working" | "blocked")
}

/// Whether a record's state is one the user should see once: `done`
/// and `error`, which carry `seen`.
fn seeable(state: &str) -> bool {
    matches!(state, "done" | "error")
}

/// How urgently a record wants the user, most urgent first; `None` for
/// one that does not.
fn urgency(record: &StatusRecord) -> Option<u8> {
    match record.state.as_str() {
        "blocked" => Some(0),
        "error" if record.seen != Some(true) => Some(1),
        "done" if record.seen != Some(true) => Some(2),
        "working" => Some(3),
        _ => None,
    }
}

fn text(value: String) -> Option<String> {
    (!value.is_empty()).then_some(value)
}

impl Records {
    /// Records loaded from a park file, oldest report first as [`all`]
    /// writes them, minus the transient ones, whose ids are returned too.
    /// A file from before `seen` has the `done` and `error` records
    /// unseen.
    ///
    /// [`all`]: Records::all
    pub fn load(records: Vec<StatusRecord>) -> (Self, Vec<String>) {
        let mut out = Records::default();
        let mut dropped = Vec::new();
        for mut record in records {
            if transient(&record) {
                dropped.push(record.id);
                continue;
            }
            if seeable(&record.state) && record.seen.is_none() {
                record.seen = Some(false);
            }
            out.insert(record);
        }
        (out, dropped)
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
            seen: seeable(state).then_some(false),
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

    /// Marks the unseen `done` and `error` records among `ids` seen, all
    /// of them when `ids` is `None`; returns them as they are now.
    pub fn ack(&mut self, ids: Option<&[String]>) -> Vec<StatusRecord> {
        let mut acked = Vec::new();
        for (id, (record, _)) in &mut self.by_id {
            if record.seen == Some(false) && ids.is_none_or(|ids| ids.contains(id)) {
                record.seen = Some(true);
                acked.push(record.clone());
            }
        }
        acked
    }

    /// The records as reported, every key of their last report, oldest
    /// report first: a park file keeps the order the attention reads.
    pub fn all(&self) -> Vec<StatusRecord> {
        let mut records: Vec<_> = self.by_id.values().collect();
        records.sort_by_key(|(_, tick)| *tick);
        records.into_iter().map(|(r, _)| r.clone()).collect()
    }

    /// The tick of the latest report; records set later have a higher one.
    pub fn tick(&self) -> u64 {
        self.tick
    }

    /// The record's `app`, else the nearest ancestor's that has one.
    fn app(&self, record: &StatusRecord) -> Option<String> {
        let app_of = |id: &str| self.by_id.get(id).and_then(|(r, _)| r.app.clone());
        if record.app.is_some() {
            return record.app.clone();
        }
        let mut id = record.id.as_str();
        while let Some(cut) = id.rfind('/') {
            id = &id[..cut];
            if let Some(app) = app_of(id) {
                return Some(app);
            }
        }
        (!record.id.is_empty()).then(|| app_of("")).flatten()
    }

    /// The records for `inspect`: `app` taken from the nearest ancestor
    /// that has one when a record lacks it.
    pub fn resolved(&self) -> Vec<StatusRecord> {
        self.resolved_since(0)
    }

    /// `resolved`, of the records set after `tick`.
    pub fn resolved_since(&self, tick: u64) -> Vec<StatusRecord> {
        self.by_id
            .values()
            .filter(|(_, set)| *set > tick)
            .map(|(record, _)| StatusRecord {
                app: self.app(record),
                ..record.clone()
            })
            .collect()
    }

    /// The most urgent record that wants the user: `blocked`, then an
    /// unseen `error`, then an unseen `done`, then `working`; among
    /// those the shallowest id (the root first), then the newest report.
    pub fn attention(&self) -> Option<Attention> {
        let depth = |id: &str| {
            if id.is_empty() {
                0
            } else {
                id.matches('/').count() + 1
            }
        };
        let (record, _) = self
            .by_id
            .values()
            .filter_map(|(record, tick)| {
                let key = (
                    urgency(record)?,
                    depth(&record.id),
                    std::cmp::Reverse(*tick),
                );
                Some((record, key))
            })
            .min_by_key(|(_, key)| *key)?;
        Some(Attention {
            state: record.state.clone(),
            kind: record.kind.clone(),
            app: self.app(record),
            msg: record.msg.clone(),
            record: record.id.clone(),
        })
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
            seen: None,
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
        let (loaded, dropped) = Records::load(vec![
            record("c", "done", "t1"),
            record("b", "error", "t2"),
            record("a", "working", "t3"),
        ]);
        let ids: Vec<_> = loaded.all().into_iter().map(|x| x.id).collect();
        assert_eq!(ids, vec!["c".to_string(), "b".to_string()], "in file order");
        assert_eq!(dropped, vec!["a".to_string()]);
        assert!(loaded.all().iter().all(|r| r.seen == Some(false)));
    }

    #[test]
    fn a_park_keeps_the_report_order_the_attention_reads() {
        let mut r = Records::default();
        r.apply(report(ProgramStatusState::Done, "b", ""), 10, "same");
        r.apply(report(ProgramStatusState::Done, "a", ""), 10, "same");
        let ids: Vec<_> = r.all().into_iter().map(|x| x.id).collect();
        assert_eq!(ids, ["b", "a"]);
        let (loaded, _) = Records::load(r.all());
        assert_eq!(loaded.attention().unwrap().record, "a");
        assert_eq!(r.attention().unwrap().record, "a");
    }

    #[test]
    fn done_and_error_stay_unseen_until_acked_or_replaced() {
        let mut r = Records::default();
        let c = r.apply(report(ProgramStatusState::Done, "a", ""), 10, "t");
        assert_eq!(c.set.unwrap().seen, Some(false));
        let c = r.apply(report(ProgramStatusState::Working, "w", ""), 10, "t");
        assert_eq!(c.set.unwrap().seen, None);
        r.apply(report(ProgramStatusState::Error, "b", ""), 10, "t");
        let acked = r.ack(Some(&["b".to_string(), "w".to_string()]));
        assert_eq!(acked.len(), 1);
        assert_eq!((acked[0].id.as_str(), acked[0].seen), ("b", Some(true)));
        assert!(r.ack(Some(&["b".to_string()])).is_empty(), "once");
        let c = r.apply(report(ProgramStatusState::Error, "b", ""), 10, "t");
        assert_eq!(c.set.unwrap().seen, Some(false), "a new report");
        let acked: Vec<_> = r.ack(None).into_iter().map(|x| x.id).collect();
        assert_eq!(acked, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn attention_is_the_most_urgent_unseen_record() {
        let mut r = Records::default();
        assert_eq!(r.attention(), None);
        r.apply(report(ProgramStatusState::Idle, "", "app"), 10, "t");
        assert_eq!(r.attention(), None, "idle never counts");
        let mut put = |state, id: &str| r.apply(report(state, id, ""), 10, "t");
        put(ProgramStatusState::Working, "w");
        put(ProgramStatusState::Done, "d1");
        put(ProgramStatusState::Done, "d2");
        put(ProgramStatusState::Error, "x/child");
        put(ProgramStatusState::Error, "x");
        let attention = |r: &Records| r.attention().map(|a| (a.state, a.record));
        assert_eq!(
            attention(&r),
            Some(("error".into(), "x".into())),
            "a parent before its newer child"
        );
        let mut blocked = report(ProgramStatusState::Blocked, "b/c", "");
        blocked.kind = ProgramStatusKind::Question;
        blocked.message = "Go?".into();
        r.apply(blocked, 10, "t");
        let a = r.attention().unwrap();
        assert_eq!(
            (
                a.state.as_str(),
                a.kind.as_deref(),
                a.msg.as_deref(),
                a.app.as_deref(),
                a.record.as_str()
            ),
            ("blocked", Some("question"), Some("Go?"), Some("app"), "b/c"),
            "blocked first, its app from the root"
        );
        r.apply(report(ProgramStatusState::Clear, "b", ""), 10, "t");
        r.ack(Some(&["x".to_string(), "x/child".to_string()]));
        assert_eq!(
            attention(&r),
            Some(("done".into(), "d2".into())),
            "the newest done"
        );
        r.ack(None);
        assert_eq!(attention(&r), Some(("working".into(), "w".into())));
        r.apply(report(ProgramStatusState::Clear, "w", ""), 10, "t");
        assert_eq!(attention(&r), None);
    }
}
