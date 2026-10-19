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
    let trimmed = |region| {
        String::from_utf8(
            formatter::format(
                &term,
                Options {
                    trim: true,
                    region,
                    ..Default::default()
                },
            )
            .unwrap(),
        )
        .unwrap()
    };
    let tail = |start_row| trimmed(Region::FromRow(start_row));
    assert_eq!(tail(4), "line5\nline6");
    assert_eq!(tail(2), "line3\nline4\nline5\nline6");
    assert_eq!(tail(0), "line1\nline2\nline3\nline4\nline5\nline6");

    let clip = |from_row, cols, rows| {
        trimmed(Region::Clip {
            from_row,
            cols,
            rows,
        })
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
    assert!(term.mode(1049).unwrap(), "the alternate screen is active");
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
        while decoder.step_available() {
            if decoder.next_page().unwrap() {
                pages += 1;
                // Live output between pages goes to the same terminal.
                decoder.terminal().unwrap().vt_write(b"\x1b]2;between\x07");
            }
        }
    }
    assert!(
        ready && decoder.finished(),
        "chunk {chunk}: ready {ready}, finished {}",
        decoder.finished()
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
        assert_eq!(restored.title(), "between", "chunk {chunk}");
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

#[test]
fn decoder_refuses_a_replaced_terminal() {
    let bytes = snapshot::encode(&filled(5000)).unwrap();
    let mut decoder = snapshot::Decoder::new().unwrap();
    decoder.push(&bytes);
    let handed_out = decoder.ready().unwrap();
    // The library still holds the terminal it returned; dropping it
    // through the `&mut` must not let the next step use freed memory.
    drop(std::mem::replace(
        handed_out,
        Terminal::new(80, 24).unwrap(),
    ));
    assert_eq!(decoder.next_page(), Err(Error::InvalidValue));
    // Nor when the replacement took the adopted terminal's address: the
    // decoder keeps that one alive, so the address stays taken.
    let mut decoder = snapshot::Decoder::new().unwrap();
    decoder.push(&bytes);
    let handed_out = decoder.ready().unwrap();
    let adopted = handed_out.raw();
    for _ in 0..64 {
        let replacement = Terminal::new(80, 24).unwrap();
        assert_ne!(replacement.raw(), adopted);
        drop(std::mem::replace(decoder.terminal().unwrap(), replacement));
    }
    assert_eq!(decoder.next_page(), Err(Error::InvalidValue));
    assert!(decoder.into_terminal().is_none());
    // What was moved out stays a terminal of its own after the decoder
    // is gone and its address could have been reused.
    let mut decoder = snapshot::Decoder::new().unwrap();
    decoder.push(&bytes);
    decoder.ready().unwrap();
    let mut stale = std::mem::replace(decoder.terminal().unwrap(), Terminal::new(5, 5).unwrap());
    let (cols, rows) = (stale.cols(), stale.rows());
    assert!(decoder.into_terminal().is_none());
    let fresh: Vec<Terminal> = (0..8).map(|_| Terminal::new(30, 3).unwrap()).collect();
    stale.vt_write(b"still mine");
    assert_eq!((stale.cols(), stale.rows()), (cols, rows));
    assert!(plain(&stale).contains("still mine"));
    assert!(fresh.iter().all(|t| !plain(t).contains("still mine")));
}

#[test]
fn decoder_reports_finish_and_releases_an_unfinished_terminal() {
    let bytes = snapshot::encode(&filled(5000)).unwrap();
    let ready = snapshot::ready_end(&bytes).unwrap();
    let mut decoder = snapshot::Decoder::new().unwrap();
    decoder.push(&bytes[..ready]);
    decoder.ready().unwrap();
    assert!(!decoder.step_available());
    assert!(!decoder.finished());
    // An unfinished terminal is released all the same (an attachment
    // that ends mid-bootstrap keeps what it has).
    let partial = decoder.into_terminal().unwrap();
    assert_eq!((partial.cols(), partial.rows()), (20, 4));
    let mut decoder = snapshot::Decoder::new().unwrap();
    decoder.push(&bytes);
    decoder.ready().unwrap();
    while decoder.step_available() && decoder.next_page().unwrap() {}
    assert!(decoder.finished());
    assert!(!decoder.step_available());
    assert!(decoder.into_terminal().is_some());
}

#[derive(Default)]
struct Recorder {
    pty: Vec<u8>,
    statuses: Vec<ProgramStatus>,
    prompts: Vec<SemanticPromptKind>,
    titles: usize,
}

struct Shared(Arc<Mutex<Recorder>>);

impl Handler for Shared {
    fn write_pty(&mut self, data: &[u8]) {
        self.0.lock().unwrap().pty.extend_from_slice(data);
    }
    fn program_status(&mut self, report: ProgramStatus) {
        self.0.lock().unwrap().statuses.push(report);
    }
    fn semantic_prompt(&mut self, kind: SemanticPromptKind) {
        self.0.lock().unwrap().prompts.push(kind);
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
    fn title_changed(&mut self) {
        self.0.lock().unwrap().titles += 1;
    }
}

fn recorded() -> (Terminal<Shared>, Arc<Mutex<Recorder>>) {
    let rec = Arc::new(Mutex::new(Recorder::default()));
    let term = Terminal::with_handler(80, 24, Shared(rec.clone())).unwrap();
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
    assert_eq!(rec.lock().unwrap().titles, 1);
    assert_eq!(term.title(), "new title");
}

#[test]
fn modes_and_colors() {
    let (mut term, rec) = recorded();
    assert!(!term.mode(2031).unwrap());
    term.vt_write(b"\x1b[?2031h");
    assert!(term.mode(2031).unwrap());
    // Colours are observed as a program would, through OSC 11's answer.
    term.vt_write(b"\x1b]11;?\x07");
    assert_eq!(rec.lock().unwrap().pty, b"", "no background set, no answer");
    term.set_color(ColorKind::Background, Some(Rgb { r: 1, g: 2, b: 3 }))
        .unwrap();
    term.vt_write(b"\x1b]11;?\x07");
    assert_eq!(
        std::mem::take(&mut rec.lock().unwrap().pty),
        b"\x1b]11;rgb:0101/0202/0303\x07"
    );
    term.vt_write(b"\x1b]11;#ff0000\x07\x1b]11;?\x07");
    assert_eq!(rec.lock().unwrap().pty, b"\x1b]11;rgb:ffff/0000/0000\x07");
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
    assert_eq!(color_scheme_report(ColorScheme::Dark), b"\x1b[?997;1n");
}

#[test]
fn the_engine_id_names_the_linked_build() {
    let mut version = ghostty_vt_sys::GhosttyString {
        ptr: std::ptr::null(),
        len: 0,
    };
    // SAFETY: the version string's output type is GhosttyString, and the
    // bytes are static.
    let bytes = unsafe {
        assert_eq!(
            ghostty_vt_sys::ghostty_build_info(
                ghostty_vt_sys::GHOSTTY_BUILD_INFO_VERSION_STRING,
                &mut version as *mut _ as *mut std::ffi::c_void,
            ),
            ghostty_vt_sys::GHOSTTY_SUCCESS
        );
        std::slice::from_raw_parts(version.ptr, version.len)
    };
    let built = std::str::from_utf8(bytes).unwrap();
    assert_eq!(ENGINE, format!("ghostty-vt@{built}"));
}

mod keys {
    use crate::Terminal;
    use crate::keys::{KeyEncoder, KeyPress, encode_paste};

    fn press(term: &Terminal, name: &str) -> Vec<u8> {
        let key = KeyPress::parse(name).unwrap_or_else(|| panic!("{name}"));
        KeyEncoder::new().unwrap().encode(term, &key).unwrap()
    }

    #[test]
    fn keys_follow_the_cursor_keys_mode() {
        let mut term = Terminal::new(80, 24).unwrap();
        assert_eq!(press(&term, "up"), b"\x1b[A");
        term.vt_write(b"\x1b[?1h");
        assert_eq!(press(&term, "up"), b"\x1bOA");
        assert_eq!(press(&term, "LEFT"), b"\x1bOD");
        term.vt_write(b"\x1b[?1l");
        assert_eq!(press(&term, "up"), b"\x1b[A");
    }

    #[test]
    fn keys_follow_the_kitty_keyboard_flags() {
        let mut term = Terminal::new(80, 24).unwrap();
        assert_eq!(press(&term, "ctrl+c"), b"\x03");
        // Disambiguate escape codes.
        term.vt_write(b"\x1b[>1u");
        assert_eq!(press(&term, "ctrl+c"), b"\x1b[99;5u");
        assert_eq!(press(&term, "esc"), b"\x1b[27u");
        term.vt_write(b"\x1b[<u");
        assert_eq!(press(&term, "esc"), b"\x1b");
    }

    #[test]
    fn names_modifiers_and_characters() {
        let term = Terminal::new(80, 24).unwrap();
        for (name, bytes) in [
            ("enter", &b"\r"[..]),
            ("tab", b"\t"),
            ("shift+tab", b"\x1b[Z"),
            ("backspace", b"\x7f"),
            ("esc", b"\x1b"),
            ("space", b" "),
            ("f5", b"\x1b[15~"),
            ("ctrl+a", b"\x01"),
            ("alt+x", b"\x1bx"),
            ("x", b"x"),
            ("X", b"X"),
            ("shift+x", b"X"),
            ("?", b"?"),
            ("+", b"+"),
            ("é", "é".as_bytes()),
        ] {
            assert_eq!(press(&term, name), bytes, "{name}");
        }
        for bad in [
            "",
            "ctrl+",
            "hyper+a",
            "ctrl+ctrl+a",
            "f13",
            "f0",
            "enterr",
            "ab",
            "\n",
        ] {
            assert!(KeyPress::parse(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn a_paste_is_bracketed_only_in_mode_2004_and_never_escapes() {
        assert_eq!(encode_paste(b"a\nb", false), b"a\rb");
        assert_eq!(encode_paste(b"a\nb", true), b"\x1b[200~a\nb\x1b[201~");
        let injected = encode_paste(b"x\x1b[201~y", true);
        assert_eq!(
            injected.iter().filter(|&&b| b == 0x1b).count(),
            2,
            "{injected:?}"
        );
    }
}
