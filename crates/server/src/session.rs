use std::borrow::Cow;
use std::collections::VecDeque;
use std::num::NonZeroU32;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ghostty_vt::{
    ColorKind, ColorScheme, Handler, ProgramStatus, ProgressState, PromptKind, Rgb, ScrollbackRow,
    SemanticPrompt, SizeReport, Terminal, color_scheme_report, formatter, snapshot,
};
use proto::{
    AttachmentInfo, Attention, Cells, Closed, ClosedReason, ErrorCode, Event, Exit, OnExit, Peer,
    Profile, Scheme, Size, SizeInfo, SizePolicy, Status, StatusReason, StreamCursor, Viewport,
};
use tokio::io::unix::AsyncFd;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, mpsc, oneshot};
use tokio::time::Instant;

use crate::attach::Ending;
use crate::commands::Commands;
use crate::park::ParkFile;
use crate::records::{Change, Records};
use crate::registry::{Fail, Registry};
use crate::stream::{MarkerKind, Stream};

pub type Shared = Arc<Mutex<Session>>;

const PROMPTS: usize = 1024;

pub const COALESCE: Duration = Duration::from_millis(25);

pub const SNAPSHOT_RETRY: Duration = Duration::from_secs(5);

pub const BELL_COALESCE: Duration = Duration::from_millis(100);

#[derive(Default)]
pub struct Effects {
    pub pty_output: Vec<u8>,
    pub changed: bool,
    pub scheme: Option<ColorScheme>,
    pub bg: Option<Rgb>,
    pub size: SizeReport,
    pub offset: u64,
    pub notices: Vec<(u64, Notice)>,
    pub held: Vec<(u64, Notice)>,
}

pub enum Notice {
    Report(ProgramStatus),
    PromptStart(PromptKind),
    CommandStarted,
    CommandEnded(Option<i32>),
    Bell,
    Notification(String, String),
    Progress(ProgressState, Option<u8>),
}

impl Effects {
    pub fn scheme_answer(&self) -> Option<ColorScheme> {
        self.scheme.or_else(|| {
            self.bg.map(|c| {
                if luminance(c) < 0.5 {
                    ColorScheme::Dark
                } else {
                    ColorScheme::Light
                }
            })
        })
    }
}

impl Handler for Effects {
    fn write_pty(&mut self, data: &[u8]) {
        self.pty_output.extend_from_slice(data);
    }

    fn color_scheme(&mut self) -> Option<ColorScheme> {
        self.scheme_answer()
    }

    fn size(&mut self) -> Option<SizeReport> {
        Some(self.size)
    }

    fn title_changed(&mut self) {
        self.changed = true;
    }

    fn pwd_changed(&mut self) {
        self.changed = true;
    }

    fn reset(&mut self) {
        self.changed = true;
    }

    fn program_status(&mut self, report: ProgramStatus) {
        self.notices.push((self.offset, Notice::Report(report)));
    }

    fn semantic_prompt(&mut self, prompt: SemanticPrompt) {
        let notice = match prompt {
            SemanticPrompt::PromptStart(kind) => Notice::PromptStart(kind),
            SemanticPrompt::OutputStart => Notice::CommandStarted,
            SemanticPrompt::InputStart | SemanticPrompt::CommandEnd => return,
        };
        self.notices.push((self.offset, notice));
    }

    fn command_end(&mut self, exit: Option<i32>) {
        self.notices.push((self.offset, Notice::CommandEnded(exit)));
    }

    fn bell(&mut self) {
        self.notices.push((self.offset, Notice::Bell));
    }

    fn desktop_notification(&mut self, title: String, body: String) {
        self.notices
            .push((self.offset, Notice::Notification(title, body)));
    }

    fn progress_report(&mut self, state: ProgressState, progress: Option<u8>) {
        self.notices
            .push((self.offset, Notice::Progress(state, progress)));
    }
}

fn pieces(bytes: &[u8]) -> impl Iterator<Item = usize> + '_ {
    let cuts = (0..bytes.len())
        .filter(move |&i| bytes[i] == 0x07 || (bytes[i] == b'\\' && i > 0 && bytes[i - 1] == 0x1b));
    cuts.map(|i| i + 1)
        .filter(move |&end| end < bytes.len())
        .chain(std::iter::once(bytes.len()))
}

#[derive(Default, PartialEq)]
pub struct Announced {
    title: String,
    pwd: String,
    attention: Option<Attention>,
    command: Option<proto::Command>,
}

pub fn announced(session: &proto::Session) -> Announced {
    Announced {
        title: session.title.clone(),
        pwd: session.pwd.clone(),
        attention: session.attention.clone(),
        command: session.command,
    }
}

/// Relative luminance per WCAG, 0 for black and 1 for white.
pub fn luminance(c: Rgb) -> f64 {
    fn lin(v: u8) -> f64 {
        let s = f64::from(v) / 255.0;
        if s <= 0.04045 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    }
    0.2126 * lin(c.r) + 0.7152 * lin(c.g) + 0.0722 * lin(c.b)
}

/// The colours OSC 10, 11 and 12 report when the profile leaves them
/// unset: Ghostty's own defaults, so that programs that query before any
/// client attached still get an answer.
pub fn default_color(kind: ColorKind) -> Rgb {
    match kind {
        ColorKind::Foreground | ColorKind::Cursor => Rgb {
            r: 0xff,
            g: 0xff,
            b: 0xff,
        },
        ColorKind::Background => Rgb {
            r: 0x28,
            g: 0x2c,
            b: 0x34,
        },
    }
}

pub fn parse_rgb(value: &str) -> Option<Rgb> {
    let hex = value.strip_prefix('#')?;
    if hex.len() != 6 || !hex.is_ascii() {
        return None;
    }
    let channel = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    Some(Rgb {
        r: channel(0)?,
        g: channel(2)?,
        b: channel(4)?,
    })
}

pub struct InputUnit {
    pub bytes: Vec<u8>,
    pub permit: OwnedSemaphorePermit,
    pub done: oneshot::Sender<()>,
}

pub struct InputQueue {
    tx: mpsc::UnboundedSender<InputUnit>,
    permits: Arc<Semaphore>,
}

impl InputQueue {
    pub fn new(max: usize) -> (Self, mpsc::UnboundedReceiver<InputUnit>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let queue = InputQueue {
            tx,
            permits: Arc::new(Semaphore::new(max)),
        };
        (queue, rx)
    }

    pub fn push(&self, bytes: Vec<u8>) -> Option<oneshot::Receiver<()>> {
        let permit = self
            .permits
            .clone()
            .try_acquire_many_owned(u32::try_from(bytes.len()).ok()?)
            .ok()?;
        let (done, wait) = oneshot::channel();
        self.tx
            .send(InputUnit {
                bytes,
                permit,
                done,
            })
            .ok()?;
        Some(wait)
    }
}

pub fn check_size(size: &Size) -> Result<(), Fail> {
    if size.cols == 0 || size.rows == 0 {
        return Err((
            ErrorCode::InvalidRequest,
            "cols and rows must be at least 1".into(),
        ));
    }
    Ok(())
}

pub struct ScreenText {
    pub all: String,
    pub history_lines: usize,
}

#[derive(Clone, Copy)]
pub struct Unparked {
    pub first: Instant,
    pub latest: Instant,
}

impl Unparked {
    pub fn at(now: Instant) -> Self {
        Unparked {
            first: now,
            latest: now,
        }
    }
}

pub struct Attachment {
    pub id: u64,
    pub client: Peer,
    pub read_only: bool,
    pub viewport: Option<Cells>,
    pub since: String,
    pub last_input: Option<String>,
    pub close: mpsc::WeakUnboundedSender<Ending>,
    pub finished: Arc<AtomicBool>,
}

impl Attachment {
    fn to_proto(&self) -> AttachmentInfo {
        AttachmentInfo {
            channel: self.id,
            client: self.client.clone(),
            read_only: self.read_only,
            viewport: self.viewport,
            since: self.since.clone(),
            last_input: self.last_input.clone(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Attachment(u64),
    Resize,
}

pub struct Pending {
    pub report: Viewport,
    pub from: u64,
    pub recorded: bool,
}

pub struct Live {
    pub pid: Option<u32>,
    pub pgid: u32,
    pub term: Terminal<Effects>,
    pub stream: Stream,
    pub master: Arc<AsyncFd<OwnedFd>>,
    pub slave_path: String,
    pub input: InputQueue,
    pub pty_eof: bool,
    pub pending: Option<Pending>,
    pub timer_running: bool,
    pub prompts: VecDeque<u64>,
    pub last_output: Instant,
}

impl Live {
    fn text_from(&self, row: ScrollbackRow) -> Result<String, ghostty_vt::Error> {
        formatter::format(
            &self.term,
            formatter::Options {
                unwrap: true,
                trim: true,
                region: formatter::Region::FromRow(row),
                ..Default::default()
            },
        )
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
    }

    pub fn text(&self) -> Result<ScreenText, ghostty_vt::Error> {
        let all = self.text_from(ScrollbackRow(0))?;
        let screen = self.text_from(self.term.screen_top()?)?;
        let history_lines = all.lines().count() - screen.lines().count();
        Ok(ScreenText { all, history_lines })
    }
}

pub struct Parked {
    pub engine: String,
    pub cols: u16,
    pub rows: u16,
    pub title: String,
    pub pwd: String,
    pub offset: u64,
    pub marker: u64,
    pub screen: String,
    pub history_lines: usize,
    pub snapshot_path: PathBuf,
}

pub enum Body {
    Live(Box<Live>),
    Parked(Parked),
}

pub struct Session {
    pub id: String,
    pub label: String,
    pub argv: Vec<String>,
    pub cwd: String,
    pub on_exit: OnExit,
    pub created_at: String,
    pub exited_at: Option<String>,
    pub exit: Option<Exit>,
    pub wake: Arc<Notify>,
    pub text_limit: usize,
    pub records_limit: usize,
    pub commands_limit: usize,
    pub attached: Vec<Attachment>,
    pub size_policy: SizePolicy,
    pub size_owner: Option<u64>,
    pub records: Records,
    pub commands: Commands,
    pub events: Vec<Event>,
    pub removed: bool,
    pub announced: Announced,
    pub last_bell: Option<Instant>,
    pub park_lock: Arc<tokio::sync::Mutex<()>>,
    pub unparked: Option<Unparked>,
    pub parked_at: Option<Instant>,
    pub park_due: Arc<Notify>,
    pub send_chain: Option<tokio::task::JoinHandle<()>>,
    pub last_generation: u64,
    pub body: Body,
}

impl Session {
    pub fn blank(limits: &proto::Limits, body: Body) -> Session {
        Session {
            id: String::new(),
            label: String::new(),
            argv: Vec::new(),
            cwd: String::new(),
            on_exit: OnExit::Remove,
            created_at: String::new(),
            exited_at: None,
            exit: None,
            wake: Arc::new(Notify::new()),
            text_limit: proto::cap(limits.text),
            records_limit: proto::cap(limits.records),
            commands_limit: proto::cap(limits.commands),
            attached: Vec::new(),
            size_policy: SizePolicy::Latest,
            size_owner: None,
            records: Records::default(),
            commands: Commands::default(),
            events: Vec::new(),
            removed: false,
            announced: Announced::default(),
            last_bell: None,
            park_lock: Arc::default(),
            unparked: None,
            parked_at: None,
            park_due: Arc::new(Notify::new()),
            send_chain: None,
            last_generation: 0,
            body,
        }
    }

    pub fn status(&self) -> Status {
        match (&self.body, &self.exited_at) {
            (_, Some(_)) => Status::Exited,
            (Body::Live(_), None) => Status::Running,
            (Body::Parked(_), None) => Status::Restored,
        }
    }

    pub fn running(&self) -> Result<&Live, Fail> {
        match (&self.body, &self.exited_at) {
            (Body::Live(live), None) => Ok(live),
            _ => Err((ErrorCode::NotRunning, "session is not running".into())),
        }
    }

    pub fn gone(&self) -> bool {
        self.removed || (self.on_exit == OnExit::Remove && self.exited_at.is_some())
    }

    pub fn live(&self) -> Option<&Live> {
        match &self.body {
            Body::Live(live) => Some(live),
            Body::Parked(_) => None,
        }
    }

    pub fn live_mut(&mut self) -> Option<&mut Live> {
        match &mut self.body {
            Body::Live(live) => Some(live),
            Body::Parked(_) => None,
        }
    }

    pub fn parked(&self) -> Option<&Parked> {
        match &self.body {
            Body::Parked(parked) => Some(parked),
            Body::Live(_) => None,
        }
    }

    pub fn cursor(&self) -> StreamCursor {
        match &self.body {
            Body::Live(live) => StreamCursor {
                offset: live.stream.offset(),
                marker: live.stream.latest_seq(),
            },
            Body::Parked(parked) => StreamCursor {
                offset: parked.offset,
                marker: parked.marker,
            },
        }
    }

    pub fn to_proto(&self, with_records: bool) -> proto::Session {
        let (pid, cols, rows, title, pwd) = match &self.body {
            Body::Live(live) => (
                live.pid,
                live.term.cols(),
                live.term.rows(),
                live.term.title(),
                pwd_path(&live.term.pwd()),
            ),
            Body::Parked(parked) => (
                None,
                parked.cols,
                parked.rows,
                parked.title.clone(),
                parked.pwd.clone(),
            ),
        };
        let StreamCursor { offset, marker } = self.cursor();
        proto::Session {
            id: self.id.clone(),
            label: self.label.clone(),
            status: self.status(),
            argv: self.argv.clone(),
            cwd: self.cwd.clone(),
            pid,
            cols,
            rows,
            title: truncate(title, self.text_limit),
            pwd: truncate(pwd, self.text_limit),
            on_exit: self.on_exit,
            created_at: self.created_at.clone(),
            exited_at: self.exited_at.clone(),
            exit: self.exit.clone(),
            offset,
            marker,
            attachments: self.attached.iter().map(Attachment::to_proto).collect(),
            command: self.commands.current(),
            program_status: with_records.then(|| self.records.resolved()),
            attention: self.records.attention(),
            size: SizeInfo {
                policy: self.size_policy,
                owner: self.size_owner,
            },
        }
    }

    pub fn touch(&mut self) {
        let now = Instant::now();
        if let Some(unparked) = &mut self.unparked {
            unparked.latest = now;
        } else {
            self.unparked = Some(Unparked::at(now));
            self.park_due.notify_one();
        }
    }

    pub fn take_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }

    /// The engine ends a sequence closed by ST at its ESC, so notices
    /// raised by bytes that end in ESC wait for the next bytes, and are
    /// placed after them when they begin with `\`.
    pub fn output(&mut self, bytes: &[u8]) {
        let Body::Live(live) = &mut self.body else {
            return;
        };
        let base = live.stream.offset();
        let effects = live.term.handler_mut();
        let after_st = u64::from(bytes.first() == Some(&b'\\'));
        let held = std::mem::take(&mut effects.held);
        effects.notices.extend(
            held.into_iter()
                .map(|(offset, notice)| (offset + after_st, notice)),
        );
        let mut start = 0;
        for end in pieces(bytes) {
            live.term.handler_mut().offset = base + end as u64;
            live.term.vt_write(&bytes[start..end]);
            start = end;
        }
        if bytes.last() == Some(&0x1b) {
            let end = base + bytes.len() as u64;
            let effects = live.term.handler_mut();
            let keep = effects
                .notices
                .iter()
                .take_while(|(o, _)| *o != end)
                .count();
            effects.held = effects.notices.split_off(keep);
        }
        live.stream.append(bytes);
        live.last_output = Instant::now();
        self.flush_effects();
        self.wake.notify_waiters();
        self.touch();
    }

    pub fn flush_effects(&mut self) {
        let Body::Live(live) = &mut self.body else {
            return;
        };
        let effects = live.term.handler_mut();
        let answers = std::mem::take(&mut effects.pty_output);
        let notices = std::mem::take(&mut effects.notices);
        let mut changed = std::mem::take(&mut effects.changed);
        if !answers.is_empty() && live.input.push(answers).is_none() {
            tracing::warn!(session = self.id, "input queue full; query answers dropped");
        }
        if notices.is_empty() && !changed {
            return;
        }
        let now = now();
        let id = self.id.clone();
        for (offset, notice) in notices {
            let event = match notice {
                Notice::Report(report) => {
                    self.apply_status_report(report, &now);
                    changed = true;
                    continue;
                }
                Notice::PromptStart(prompt) => {
                    changed |= self.prompt_started(prompt, offset);
                    continue;
                }
                Notice::CommandStarted => {
                    changed |= self.mark(SemanticPrompt::OutputStart, None, offset);
                    Event::CommandStarted {
                        id: id.clone(),
                        offset,
                    }
                }
                Notice::CommandEnded(exit) => {
                    changed |= self.mark(SemanticPrompt::CommandEnd, exit, offset);
                    Event::CommandEnded {
                        id: id.clone(),
                        offset,
                        exit,
                    }
                }
                Notice::Bell => {
                    let at = Instant::now();
                    if self
                        .last_bell
                        .is_some_and(|last| at.duration_since(last) < BELL_COALESCE)
                    {
                        continue;
                    }
                    self.last_bell = Some(at);
                    Event::Bell { id: id.clone() }
                }
                Notice::Notification(title, body) => Event::Notification {
                    id: id.clone(),
                    title: truncate(title, self.text_limit),
                    body: truncate(body, self.text_limit),
                },
                Notice::Progress(state, progress) => Event::Progress {
                    id: id.clone(),
                    state: match state {
                        ProgressState::Remove => proto::ProgressState::Remove,
                        ProgressState::Set => proto::ProgressState::Set,
                        ProgressState::Error => proto::ProgressState::Error,
                        ProgressState::Indeterminate => proto::ProgressState::Indeterminate,
                        ProgressState::Pause => proto::ProgressState::Pause,
                    },
                    progress,
                },
            };
            self.events.push(event);
        }
        if changed {
            self.announce();
        }
    }

    fn apply_status_report(&mut self, report: ProgramStatus, now: &str) {
        let (removed, reason) = match self.records.apply(report, self.records_limit, now) {
            Change::Cleared(ids) => (ids, StatusReason::Clear),
            Change::Set { record, evicted } => {
                self.events.push(Event::ProgramStatusChanged {
                    id: self.id.clone(),
                    record,
                    reason: StatusReason::Report,
                });
                (evicted.into_iter().collect(), StatusReason::Evicted)
            }
        };
        if !removed.is_empty() {
            self.events.push(Event::ProgramStatusRemoved {
                id: self.id.clone(),
                record_ids: removed,
                reason,
            });
        }
    }

    fn prompt_started(&mut self, prompt: PromptKind, offset: u64) -> bool {
        let mut changed = self.mark(SemanticPrompt::PromptStart(prompt), None, offset);
        if let Body::Live(live) = &mut self.body {
            let start = live.stream.retained_start();
            live.prompts.retain(|&o| o >= start);
            if live.prompts.len() >= PROMPTS {
                live.prompts.pop_front();
            }
            live.prompts.push_back(offset);
        }
        changed |= self.drop_transient_records(StatusReason::Prompt);
        changed
    }

    pub fn announce(&mut self) {
        let session = self.to_proto(false);
        let announced = announced(&session);
        if announced != self.announced {
            self.announced = announced;
            self.events.push(Event::SessionChanged { session });
        }
    }

    fn mark(&mut self, mark: SemanticPrompt, exit: Option<i32>, offset: u64) -> bool {
        let before = self.commands.current();
        self.commands.apply(mark, exit, offset, self.commands_limit)
            && self.commands.current() != before
    }

    fn drop_transient_records(&mut self, reason: StatusReason) -> bool {
        let removed = self.records.drop_transient();
        if removed.is_empty() {
            return false;
        }
        self.events.push(Event::ProgramStatusRemoved {
            id: self.id.clone(),
            record_ids: removed,
            reason,
        });
        true
    }

    pub fn ack(&mut self, ids: Option<&[String]>) -> Vec<String> {
        let acked = self.records.ack(ids);
        let record_ids = acked.iter().map(|r| r.id.clone()).collect();
        for record in acked {
            self.events.push(Event::ProgramStatusChanged {
                id: self.id.clone(),
                record,
                reason: StatusReason::Ack,
            });
        }
        self.announce();
        record_ids
    }

    pub fn size(&self) -> Size {
        let (cols, rows) = match &self.body {
            Body::Live(live) => (live.term.cols(), live.term.rows()),
            Body::Parked(parked) => (parked.cols, parked.rows),
        };
        Size {
            cols,
            rows,
            cell_width: None,
            cell_height: None,
        }
    }

    pub fn record_viewport(&mut self, id: u64, size: &Size) -> bool {
        let cells = Some(Cells {
            cols: size.cols,
            rows: size.rows,
        });
        match self.attached.iter_mut().find(|a| a.id == id) {
            Some(a) if a.viewport != cells => {
                a.viewport = cells;
                true
            }
            _ => false,
        }
    }

    fn smallest(&self) -> Option<(u16, u16)> {
        let mut viewports = self.attached.iter().filter_map(|a| a.viewport);
        let first = viewports.next()?;
        Some(viewports.fold((first.cols, first.rows), |(cols, rows), v| {
            (cols.min(v.cols), rows.min(v.rows))
        }))
    }

    pub fn apply_report(&mut self, report: Viewport, from: Source) -> bool {
        let Body::Live(live) = &mut self.body else {
            return false;
        };
        let recorded = live.pending.take().is_some_and(|p| p.recorded);
        if self.exited_at.is_some() {
            return recorded;
        }
        let Viewport {
            size:
                Size {
                    cols,
                    rows,
                    cell_width,
                    cell_height,
                },
            profile,
        } = report;
        if let Some(profile) = profile {
            self.set_profile(&profile);
        }
        let current = self.size();
        let owner = self.size_owner;
        let (cols, rows) = match (from, self.size_policy) {
            (Source::Resize, _) => {
                self.size_owner = None;
                (cols, rows)
            }
            (Source::Attachment(id), SizePolicy::Latest) => {
                self.size_owner = self.attached.iter().any(|a| a.id == id).then_some(id);
                (cols, rows)
            }
            (Source::Attachment(_), SizePolicy::Smallest) => {
                self.smallest().unwrap_or((current.cols, current.rows))
            }
            (Source::Attachment(_), SizePolicy::Fixed) => (current.cols, current.rows),
        };
        let resized = self.resize_to(cols, rows, cell_width, cell_height);
        recorded || resized || owner != self.size_owner
    }

    fn resize_to(
        &mut self,
        cols: u16,
        rows: u16,
        cell_width: Option<u32>,
        cell_height: Option<u32>,
    ) -> bool {
        let Body::Live(live) = &mut self.body else {
            return false;
        };
        let current = live.term.handler().size;
        let cell_width = cell_width.map_or(current.cell_width, NonZeroU32::new);
        let cell_height = cell_height.map_or(current.cell_height, NonZeroU32::new);
        let changed = (live.term.cols(), live.term.rows()) != (cols, rows);
        if changed
            && live
                .term
                .resize(
                    cols,
                    rows,
                    cell_width.map_or(0, NonZeroU32::get),
                    cell_height.map_or(0, NonZeroU32::get),
                )
                .is_err()
        {
            return false;
        }
        live.term.handler_mut().size = SizeReport {
            cols,
            rows,
            cell_width,
            cell_height,
        };
        if !changed {
            return false;
        }
        let _ = crate::pty::set_size(&live.master, cols, rows);
        live.stream.add_marker(MarkerKind::Resized { cols, rows });
        self.flush_effects();
        self.wake.notify_waiters();
        self.touch();
        true
    }

    pub fn set_policy(&mut self, policy: SizePolicy) -> bool {
        let before = (self.size_policy, self.size_owner);
        self.size_policy = policy;
        self.size_owner = None;
        let resized = match (policy, self.smallest()) {
            (SizePolicy::Smallest, Some((cols, rows))) => self.resize_to(cols, rows, None, None),
            _ => false,
        };
        let changed = resized || before != (self.size_policy, self.size_owner);
        if changed {
            self.touch();
        }
        changed
    }

    pub fn remove_attachment(&mut self, id: u64) {
        self.attached.retain(|a| a.id != id);
        if self.size_owner == Some(id) {
            self.size_owner = None;
        }
        if self.size_policy == SizePolicy::Smallest
            && self.running().is_ok()
            && let Some((cols, rows)) = self.smallest()
        {
            self.resize_to(cols, rows, None, None);
        }
    }

    pub fn set_profile(&mut self, profile: &Profile) {
        let id = self.id.clone();
        let Body::Live(live) = &mut self.body else {
            return;
        };
        let before = live.term.handler().scheme_answer();
        let effects = live.term.handler_mut();
        effects.scheme = profile.scheme.map(|s| match s {
            Scheme::Light => ColorScheme::Light,
            Scheme::Dark => ColorScheme::Dark,
        });
        effects.bg = profile.bg.as_deref().and_then(parse_rgb);
        for (kind, value) in [
            (ColorKind::Foreground, &profile.fg),
            (ColorKind::Background, &profile.bg),
            (ColorKind::Cursor, &profile.cursor),
        ] {
            let color = value
                .as_deref()
                .and_then(parse_rgb)
                .unwrap_or(default_color(kind));
            let _ = live.term.set_color(kind, Some(color));
        }
        let after = live.term.handler().scheme_answer();
        if after != before
            && let Some(scheme) = after
            && live.term.mode(2031).unwrap_or(false)
            && live.input.push(color_scheme_report(scheme)).is_none()
        {
            tracing::warn!(session = id, "input queue full; scheme report dropped");
        }
    }

    pub fn maybe_end(&mut self) -> Option<Exit> {
        if self.exited_at.is_some() {
            return None;
        }
        let exit = self.exit.clone()?;
        let Body::Live(live) = &mut self.body else {
            return None;
        };
        if !live.pty_eof {
            return None;
        }
        self.exited_at = Some(now());
        let effects = live.term.handler_mut();
        let held = std::mem::take(&mut effects.held);
        effects.notices.extend(held);
        self.flush_effects();
        let Body::Live(live) = &mut self.body else {
            return None;
        };
        live.stream
            .add_marker(MarkerKind::Exited { exit: exit.clone() });
        self.drop_transient_records(StatusReason::Exit);
        self.announced = announced(&self.to_proto(false));
        self.wake.notify_waiters();
        self.park_due.notify_one();
        Some(exit)
    }

    pub fn read_text(&self, scrollback: usize) -> Result<String, ghostty_vt::Error> {
        let (text, history_lines) = match &self.body {
            Body::Live(live) => blocking(|| {
                // When the entire screen is blank, trimming also removes
                // trailing empty history lines. Keep counting from that
                // trimmed history for a request that includes scrollback.
                if scrollback > 0
                    && scrollback != usize::MAX
                    && live.text_from(live.term.history_start(0)?)?.is_empty()
                {
                    return live
                        .text()
                        .map(|text| (Cow::Owned(text.all), text.history_lines));
                }
                let start = if scrollback == usize::MAX {
                    ScrollbackRow(0)
                } else {
                    live.term.history_start(scrollback)?
                };
                live.text_from(start).map(|text| (Cow::Owned(text), 0))
            })?,
            Body::Parked(parked) => (Cow::Borrowed(parked.screen.as_str()), parked.history_lines),
        };
        let skip = history_lines.saturating_sub(scrollback);
        Ok(text.lines().skip(skip).collect::<Vec<_>>().join("\n"))
    }

    pub fn park_file(
        &self,
        live: &Live,
        at: jiff::Timestamp,
    ) -> Result<(ParkFile, String), ghostty_vt::Error> {
        let ScreenText { all, history_lines } = live.text()?;
        let file = ParkFile {
            version: crate::park::VERSION,
            engine: ghostty_vt::ENGINE.into(),
            parked_at: at.to_string(),
            session: proto::Session {
                attachments: Vec::new(),
                pid: None,
                program_status: Some(self.records.all()),
                attention: None,
                size: SizeInfo {
                    policy: self.size_policy,
                    owner: None,
                },
                ..self.to_proto(false)
            },
            history_lines,
            commands: self.commands.all(),
        };
        Ok((file, all))
    }

    pub fn attachments_finished(&self) -> bool {
        self.attached
            .iter()
            .all(|a| a.finished.load(Ordering::Acquire))
    }

    pub fn close_attachments(&self, reason: ClosedReason) {
        self.close_attachments_where(reason, |_| true);
    }

    pub fn close_finished_attachments(&self, reason: ClosedReason) {
        self.close_attachments_where(reason, |a| a.finished.load(Ordering::Acquire));
    }

    fn close_attachments_where(&self, reason: ClosedReason, closes: impl Fn(&Attachment) -> bool) {
        for a in &self.attached {
            if closes(a)
                && let Some(close) = a.close.upgrade()
            {
                let _ = close.send(Ending::Closed(Closed {
                    reason,
                    message: None,
                }));
            }
        }
    }
}

pub fn report_coalesced(registry: &Arc<Registry>, session: &Shared, from: u64, report: Viewport) {
    let mut s = session.lock().unwrap();
    let recorded = s.record_viewport(from, &report.size);
    let Some(live) = s.live_mut() else {
        return;
    };
    live.pending = Some(match live.pending.take() {
        Some(earlier) => Pending {
            report: Viewport {
                size: Size {
                    cell_width: report.size.cell_width.or(earlier.report.size.cell_width),
                    cell_height: report.size.cell_height.or(earlier.report.size.cell_height),
                    ..report.size
                },
                profile: report.profile.or(earlier.report.profile),
            },
            from,
            recorded: recorded || earlier.recorded,
        },
        None => Pending {
            report,
            from,
            recorded,
        },
    });
    if live.timer_running {
        return;
    }
    live.timer_running = true;
    drop(s);
    let registry = registry.clone();
    let session = session.clone();
    tokio::spawn(async move {
        tokio::time::sleep(COALESCE).await;
        let (changed, events) = {
            let mut s = session.lock().unwrap();
            let pending = s.live_mut().and_then(|live| {
                live.timer_running = false;
                live.pending.take()
            });
            let changed = pending
                .is_some_and(|p| s.apply_report(p.report, Source::Attachment(p.from)) || p.recorded)
                .then(|| s.to_proto(false));
            (changed, s.take_events())
        };
        registry.emit_session(events, changed);
    });
}

/// The path of an OSC 7 value: what follows the host of any
/// `scheme://host/path` (`file://` from most shells, `kitty-shell-cwd://`
/// from Ghostty's integration), percent-decoded; a bare path as is.
pub fn pwd_path(value: &str) -> String {
    let path = match value.split_once("://") {
        Some((_, rest)) => rest.find('/').map_or("", |i| &rest[i..]),
        None => value,
    };
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let decoded = match bytes.get(i + 1..i + 3) {
            Some(hex) if bytes[i] == b'%' => std::str::from_utf8(hex)
                .ok()
                .and_then(|hex| u8::from_str_radix(hex, 16).ok()),
            _ => None,
        };
        if let Some(byte) = decoded {
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn truncate(mut text: String, max: usize) -> String {
    text.truncate(text.floor_char_boundary(max));
    text
}

pub fn printable(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

pub fn now() -> String {
    jiff::Timestamp::now().to_string()
}

pub fn blocking<T>(work: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::current().runtime_flavor() {
        tokio::runtime::RuntimeFlavor::CurrentThread => work(),
        _ => tokio::task::block_in_place(work),
    }
}

pub async fn snapshot_with_retry<T>(
    session: &Shared,
    retry: Duration,
    mut take: impl FnMut(&Session, &Live, Vec<u8>) -> T,
) -> Result<Option<T>, ghostty_vt::Error> {
    let deadline = tokio::time::Instant::now() + retry;
    let wake = session.lock().unwrap().wake.clone();
    loop {
        let notified = wake.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let taken = {
            let s = session.lock().unwrap();
            let Some(live) = s.live() else {
                return Ok(None);
            };
            blocking(|| snapshot::encode(&live.term).map(|bytes| take(&s, live, bytes)))
        };
        match taken {
            Ok(value) => return Ok(Some(value)),
            Err(ghostty_vt::Error::LimitExceeded) if tokio::time::Instant::now() < deadline => {
                tokio::select! {
                    () = notified => {}
                    () = tokio::time::sleep_until(deadline) => {}
                }
            }
            Err(err) => return Err(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pwd_strips_scheme_and_host() {
        assert_eq!(pwd_path("file://mac.local/Users/me"), "/Users/me");
        assert_eq!(pwd_path("file:///tmp"), "/tmp");
        assert_eq!(pwd_path("/plain"), "/plain");
        assert_eq!(
            pwd_path("kitty-shell-cwd://mac.local/Users/me/a%20b"),
            "/Users/me/a b"
        );
        assert_eq!(pwd_path("file:///x%2"), "/x%2");
    }

    #[test]
    fn luminance_splits_dark_and_light() {
        assert!(luminance(Rgb { r: 0, g: 0, b: 0 }) < 0.5);
        assert!(
            luminance(Rgb {
                r: 255,
                g: 255,
                b: 255
            }) > 0.5
        );
        assert_eq!(
            parse_rgb("#1e1e2e"),
            Some(Rgb {
                r: 0x1e,
                g: 0x1e,
                b: 0x2e
            })
        );
        assert_eq!(parse_rgb("1e1e2e"), None);
    }
}
