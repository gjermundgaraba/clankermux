use std::cell::RefCell;
use std::rc::Rc;

use crate::formatter::{self, Format, Options};
use crate::snapshot;
use crate::*;

fn plain(term: &Term) -> String {
    String::from_utf8(formatter::format(term, Options::default()).unwrap()).unwrap()
}

#[test]
fn writes_and_formats() {
    let mut term = Terminal::new(10, 3).unwrap();
    term.vt_write(b"hello\r\nworld");
    assert_eq!(plain(&term).trim_end(), "hello\nworld");
    assert_eq!(term.cursor(), (5, 1));
    assert_eq!((term.cols(), term.rows()), (10, 3));
}

fn filled(rows: usize) -> Terminal {
    let mut term = Terminal::new(20, 4).unwrap();
    for i in 0..rows {
        term.vt_write(format!("row {i}\r\n").as_bytes());
    }
    term
}

#[test]
fn snapshot_round_trip() {
    let mut term = filled(10);
    term.vt_write(b"\x1b]2;title\x07");
    let bytes = snapshot::encode(&term).unwrap();
    assert_eq!(&bytes[..8], b"GHOSTSNP");

    let restored = snapshot::decode(&bytes).unwrap();
    assert_eq!(plain(&restored), plain(&term));
    assert_eq!(restored.title(), "title");
    assert_eq!(restored.scrollback_rows(), term.scrollback_rows());
}

/// Decodes `bytes` by steps, pushing `chunk` bytes at a time, and returns
/// the restored terminal and the number of history pages.
fn decode_in_chunks(bytes: &[u8], chunk: usize) -> (Terminal, usize) {
    let mut decoder = snapshot::Decoder::new().unwrap();
    let mut pos = 0;
    let mut pages = 0;
    let mut ready = false;
    let mut finished = false;
    while pos < bytes.len() {
        let end = (pos + chunk).min(bytes.len());
        decoder.push(&bytes[pos..end]);
        pos = end;
        if !ready {
            if !decoder.ready_available() {
                continue;
            }
            decoder.ready().unwrap();
            ready = true;
        }
        while !finished && decoder.step_available() {
            if decoder.next_page().unwrap() {
                pages += 1;
                // Live output between pages goes to the same terminal.
                decoder.terminal().unwrap().vt_write(b"");
            } else {
                finished = true;
            }
        }
    }
    assert!(
        ready && finished,
        "chunk {chunk}: ready {ready}, finished {finished}"
    );
    assert_eq!(decoder.buffered(), 0);
    (decoder.into_terminal().unwrap(), pages)
}

#[test]
fn incremental_decode_at_arbitrary_splits() {
    let term = filled(5000);
    let bytes = snapshot::encode(&term).unwrap();
    for chunk in [1, 7, 4096, bytes.len()] {
        let (restored, pages) = decode_in_chunks(&bytes, chunk);
        assert!(pages >= 1, "chunk {chunk}");
        assert_eq!(
            restored.scrollback_rows(),
            term.scrollback_rows(),
            "chunk {chunk}"
        );
        assert_eq!(plain(&restored), plain(&term), "chunk {chunk}");
    }
}

#[test]
fn decoder_keeps_its_terminal_until_released() {
    let bytes = snapshot::encode(&filled(5000)).unwrap();
    let mut decoder = snapshot::Decoder::new().unwrap();
    decoder.push(&bytes);
    assert!(decoder.terminal().is_none());
    decoder.ready().unwrap();
    assert_eq!(decoder.terminal().unwrap().rows(), 4);
    // Dropping the decoder before FINISH must leave a usable terminal.
    let mut term = decoder.into_terminal().unwrap();
    term.vt_write(b"after");
    assert!(plain(&term).contains("after"));
}

#[derive(Default)]
struct Recorder {
    pty: Vec<u8>,
    statuses: Vec<ProgramStatus>,
    prompts: Vec<SemanticPromptKind>,
    titles: Vec<String>,
    resize: Option<(u16, u16)>,
    clipboard: Vec<ClipboardContent>,
}

struct Shared(Rc<RefCell<Recorder>>);

impl Handler for Shared {
    fn write_pty(&mut self, data: &[u8]) {
        self.0.borrow_mut().pty.extend_from_slice(data);
    }
    fn program_status(&mut self, report: ProgramStatus) {
        self.0.borrow_mut().statuses.push(report);
    }
    fn semantic_prompt(&mut self, event: SemanticPrompt) {
        self.0.borrow_mut().prompts.push(event.kind);
    }
    fn color_scheme(&mut self) -> Option<ColorScheme> {
        Some(ColorScheme::Dark)
    }
    fn size(&mut self) -> Option<SizeReport> {
        Some(SizeReport {
            cols: 80,
            rows: 24,
            cell_width: 9,
            cell_height: 18,
        })
    }
    fn clipboard_write(
        &mut self,
        _location: ClipboardLocation,
        contents: &[ClipboardContent],
    ) -> bool {
        self.0.borrow_mut().clipboard.extend_from_slice(contents);
        true
    }
    fn resize_window(&mut self, cols: u16, rows: u16) {
        self.0.borrow_mut().resize = Some((cols, rows));
    }
    fn title_changed(&mut self, term: &Term) {
        self.0.borrow_mut().titles.push(term.title());
    }
}

fn recorded() -> (Terminal, Rc<RefCell<Recorder>>) {
    let rec = Rc::new(RefCell::new(Recorder::default()));
    let mut term = Terminal::new(80, 24).unwrap();
    term.set_handler(Box::new(Shared(rec.clone())));
    (term, rec)
}

#[test]
fn effects_reach_the_handler() {
    let (mut term, rec) = recorded();
    term.set_xtversion("clankermux test");
    term.vt_write(b"\x1b[c");
    assert!(
        rec.borrow().pty.starts_with(b"\x1b[?"),
        "DA1 answer: {:?}",
        rec.borrow().pty
    );
    rec.borrow_mut().pty.clear();

    term.vt_write(b"\x1b[>q");
    assert_eq!(rec.borrow().pty, b"\x1bP>|clankermux test\x1b\\");
    rec.borrow_mut().pty.clear();

    term.vt_write(b"\x1b[?996n");
    assert_eq!(rec.borrow().pty, b"\x1b[?997;1n");
    rec.borrow_mut().pty.clear();

    term.vt_write(b"\x1b[18t");
    assert_eq!(rec.borrow().pty, b"\x1b[8;24;80t");

    term.vt_write(b"\x1b]7501;state=working:id=abc:title=QnVpbGQ=\x07");
    {
        let statuses = &rec.borrow().statuses;
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].state, ProgramStatusState::Working);
        assert_eq!(statuses[0].id, "abc");
        assert_eq!(statuses[0].title, "Build");
    }

    term.vt_write(b"\x1b]133;A\x07");
    assert_eq!(rec.borrow().prompts, [SemanticPromptKind::PromptStart]);

    term.vt_write(b"\x1b]2;new title\x07");
    assert_eq!(rec.borrow().titles, ["new title"]);

    term.vt_write(b"\x1b[8;30;100t");
    assert_eq!(rec.borrow().resize, Some((100, 30)));

    term.vt_write(b"\x1b]52;c;aGVsbG8=\x07");
    let clipboard = &rec.borrow().clipboard;
    assert_eq!(clipboard.len(), 1);
    assert_eq!(clipboard[0].data, b"hello");
}

#[test]
fn modes_and_colors() {
    let mut term = Terminal::new(10, 2).unwrap();
    assert!(!term.mode(2031).unwrap());
    term.vt_write(b"\x1b[?2031h");
    assert!(term.mode(2031).unwrap());
    assert_eq!(term.color(ColorKind::Background), None);
    term.set_color(ColorKind::Background, Some(Rgb { r: 1, g: 2, b: 3 }))
        .unwrap();
    assert_eq!(
        term.color(ColorKind::Background),
        Some(Rgb { r: 1, g: 2, b: 3 })
    );
    term.vt_write(b"\x1b]11;#ff0000\x07");
    assert_eq!(
        term.color(ColorKind::Background),
        Some(Rgb { r: 255, g: 0, b: 0 })
    );
    term.vt_write(b"\x1b[1mbold");
    let vt = formatter::format(
        &term,
        Options {
            format: Format::Vt,
            ..Options::default()
        },
    )
    .unwrap();
    assert!(!vt.is_empty());
    assert!(ENGINE.starts_with("ghostty-vt@"));
}
