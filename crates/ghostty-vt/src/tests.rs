use std::sync::{Arc, Mutex};

use crate::formatter::{self, Format, Options, Region};
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
fn formats_a_tail_of_rows() {
    let mut term = Terminal::new(10, 3).unwrap();
    for i in 1..=6 {
        term.vt_write(format!("line{i}\r\n").as_bytes());
    }
    // Three rows of history, then the screen: line4, line5, line6 and a
    // blank cursor row.
    assert_eq!(term.scrollback_rows(), 4);
    let tail = |start_row| {
        String::from_utf8(
            formatter::format(
                &term,
                Options {
                    trim: true,
                    region: Region::FromRow(start_row),
                    ..Default::default()
                },
            )
            .unwrap(),
        )
        .unwrap()
    };
    assert_eq!(tail(4), "line5\nline6");
    assert_eq!(tail(2), "line3\nline4\nline5\nline6");
    assert_eq!(tail(0), "line1\nline2\nline3\nline4\nline5\nline6");

    let clip = |from_row, cols, rows| {
        String::from_utf8(
            formatter::format(
                &term,
                Options {
                    trim: true,
                    region: Region::Clip {
                        from_row,
                        cols,
                        rows,
                    },
                    ..Default::default()
                },
            )
            .unwrap(),
        )
        .unwrap()
    };
    assert_eq!(clip(0, 3, 2), "lin\nlin");
    assert_eq!(clip(1, 10, 2), "line6");
    assert_eq!(clip(9, 10, 9), "", "clamped to the screen");
}

#[test]
fn vt_format_carries_modes_not_history() {
    let mut term = Terminal::new(10, 3).unwrap();
    term.vt_write(b"one\r\ntwo\r\nthree\r\nfour\x1b[?1049h\x1b[?2004halt");
    let vt = formatter::format(
        &term,
        Options {
            format: Format::Vt,
            modes: true,
            cursor: true,
            ..Default::default()
        },
    )
    .unwrap();
    let vt = String::from_utf8(vt).unwrap();
    assert!(vt.contains("\x1b[?1049h"), "{vt:?}");
    assert!(vt.contains("\x1b[?2004h"), "{vt:?}");
    assert!(vt.contains("alt"), "{vt:?}");
    assert!(
        !vt.contains("four"),
        "the primary screen is not formatted: {vt:?}"
    );
    assert_eq!(term.active_screen(), Screen::Alternate);
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

struct Shared(Arc<Mutex<Recorder>>);

impl Handler for Shared {
    fn write_pty(&mut self, data: &[u8]) {
        self.0.lock().unwrap().pty.extend_from_slice(data);
    }
    fn program_status(&mut self, report: ProgramStatus) {
        self.0.lock().unwrap().statuses.push(report);
    }
    fn semantic_prompt(&mut self, event: SemanticPrompt) {
        self.0.lock().unwrap().prompts.push(event.kind);
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
        self.0.lock().unwrap().clipboard.extend_from_slice(contents);
        true
    }
    fn resize_window(&mut self, cols: u16, rows: u16) {
        self.0.lock().unwrap().resize = Some((cols, rows));
    }
    fn title_changed(&mut self, term: &Term) {
        self.0.lock().unwrap().titles.push(term.title());
    }
}

fn recorded() -> (Terminal<Shared>, Arc<Mutex<Recorder>>) {
    let rec = Arc::new(Mutex::new(Recorder::default()));
    let term = Terminal::new(80, 24)
        .unwrap()
        .with_handler(Shared(rec.clone()));
    (term, rec)
}

#[test]
fn effects_reach_the_handler() {
    let (mut term, rec) = recorded();
    term.set_xtversion("clankermux test");
    term.vt_write(b"\x1b[c");
    assert!(
        rec.lock().unwrap().pty.starts_with(b"\x1b[?"),
        "DA1 answer: {:?}",
        rec.lock().unwrap().pty
    );
    rec.lock().unwrap().pty.clear();

    term.vt_write(b"\x1b[>q");
    assert_eq!(rec.lock().unwrap().pty, b"\x1bP>|clankermux test\x1b\\");
    rec.lock().unwrap().pty.clear();

    term.vt_write(b"\x1b[?996n");
    assert_eq!(rec.lock().unwrap().pty, b"\x1b[?997;1n");
    rec.lock().unwrap().pty.clear();

    term.vt_write(b"\x1b[18t");
    assert_eq!(rec.lock().unwrap().pty, b"\x1b[8;24;80t");

    term.vt_write(b"\x1b]7501;state=working:id=abc:title=QnVpbGQ=\x07");
    {
        let statuses = &rec.lock().unwrap().statuses;
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].state, ProgramStatusState::Working);
        assert_eq!(statuses[0].id, "abc");
        assert_eq!(statuses[0].title, "Build");
    }

    term.vt_write(b"\x1b]133;A\x07");
    assert_eq!(
        rec.lock().unwrap().prompts,
        [SemanticPromptKind::PromptStart]
    );

    term.vt_write(b"\x1b]2;new title\x07");
    assert_eq!(rec.lock().unwrap().titles, ["new title"]);

    term.vt_write(b"\x1b[8;30;100t");
    assert_eq!(rec.lock().unwrap().resize, Some((100, 30)));

    term.vt_write(b"\x1b]52;c;aGVsbG8=\x07");
    let clipboard = &rec.lock().unwrap().clipboard;
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
    assert_eq!(color_scheme_report(ColorScheme::Dark), b"\x1b[?997;1n");
}
