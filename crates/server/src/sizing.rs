//! A session's size policy and, under `latest`, the owner, and the sizes
//! they give from the open attachments' viewports. The session applies
//! the sizes to its terminal and PTY.

use proto::{Cells, SizePolicy};

/// Where a viewport report comes from.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Attachment(u64),
    /// `resize`, applied under every policy.
    Resize,
}

pub struct Sizing {
    policy: SizePolicy,
    /// Under `latest`, the attachment whose report is current.
    owner: Option<u64>,
    /// The open attachments, oldest first, with their latest viewport.
    attached: Vec<(u64, Option<Cells>)>,
}

impl Sizing {
    pub fn new(policy: SizePolicy) -> Self {
        Sizing {
            policy,
            owner: None,
            attached: Vec::new(),
        }
    }

    pub fn policy(&self) -> SizePolicy {
        self.policy
    }

    pub fn owner(&self) -> Option<u64> {
        self.owner
    }

    /// The open attachments, oldest first, with their latest viewport.
    pub fn attached(&self) -> &[(u64, Option<Cells>)] {
        &self.attached
    }

    /// Attachment `id`'s latest viewport.
    pub fn viewport(&self, id: u64) -> Option<Cells> {
        self.attached.iter().find(|(a, _)| *a == id)?.1
    }

    /// Attachment `id` opens, with no viewport yet.
    pub fn open(&mut self, id: u64) {
        self.attached.push((id, None));
    }

    /// Records open attachment `id`'s viewport; true when it changed.
    pub fn record(&mut self, id: u64, cells: Cells) -> bool {
        match self.attached.iter_mut().find(|(a, _)| *a == id) {
            Some((_, v)) if *v != Some(cells) => {
                *v = Some(cells);
                true
            }
            _ => false,
        }
    }

    /// Attachment `id` closes: it no longer owns the size. Returns the
    /// size to apply: under `smallest`, that of those left.
    pub fn close(&mut self, id: u64) -> Option<(u16, u16)> {
        self.attached.retain(|(a, _)| *a != id);
        if self.owner == Some(id) {
            self.owner = None;
        }
        match self.policy {
            SizePolicy::Smallest => self.smallest(),
            _ => None,
        }
    }

    /// The size a report gives, with the session at `current`: the report's
    /// under `latest` and for `resize`, the smallest recorded under
    /// `smallest`, the current one under `fixed`. Under `latest` the
    /// reporting attachment owns the size while it is open; after a
    /// `resize` none does.
    pub fn apply(&mut self, from: Source, report: (u16, u16), current: (u16, u16)) -> (u16, u16) {
        match (from, self.policy) {
            (Source::Resize, _) => {
                self.owner = None;
                report
            }
            (Source::Attachment(id), SizePolicy::Latest) => {
                // A coalesced report may outlive its attachment.
                self.owner = self.attached.iter().any(|(a, _)| *a == id).then_some(id);
                report
            }
            (Source::Attachment(_), SizePolicy::Smallest) => self.smallest().unwrap_or(current),
            (Source::Attachment(_), SizePolicy::Fixed) => current,
        }
    }

    /// Sets the policy, with no owner left. Returns the size to apply:
    /// under `smallest`, the smallest recorded; the others wait for the
    /// next report.
    pub fn set_policy(&mut self, policy: SizePolicy) -> Option<(u16, u16)> {
        self.policy = policy;
        self.owner = None;
        match policy {
            SizePolicy::Smallest => self.smallest(),
            _ => None,
        }
    }

    /// The smallest columns and rows among the recorded viewports.
    fn smallest(&self) -> Option<(u16, u16)> {
        let mut viewports = self.attached.iter().filter_map(|(_, v)| *v);
        let first = viewports.next()?;
        Some(viewports.fold((first.cols, first.rows), |(cols, rows), v| {
            (cols.min(v.cols), rows.min(v.rows))
        }))
    }
}
