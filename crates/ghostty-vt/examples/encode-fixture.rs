//! Writes snapshot fixtures for the cross-engine test: each `<name>.ghostsnp`
//! is a native-encoded snapshot and `<name>.txt` the plain-text screen the
//! native formatter produces for it. `engine.txt` holds the native engine id.

use std::fs;
use std::path::PathBuf;

use ghostty_vt::formatter::{self, Options};
use ghostty_vt::{ENGINE, Terminal, snapshot};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = PathBuf::from(
        std::env::args()
            .nth(1)
            .ok_or("usage: encode-fixture <dir>")?,
    );
    fs::create_dir_all(&dir)?;
    fs::write(dir.join("engine.txt"), ENGINE)?;

    let mut basic = Terminal::new(20, 5)?;
    for i in 1..=12 {
        basic.vt_write(format!("line {i} \x1b[1;31mred\x1b[0m\r\n").as_bytes());
    }
    basic.vt_write(b"\x1b]2;fixture\x07cursor here");
    write(&dir, "basic", &basic)?;

    // History spanning several pages, decoded page by page in the browser.
    let mut history = Terminal::new(20, 5)?;
    for i in 1..=5000 {
        history.vt_write(format!("history {i}\r\n").as_bytes());
    }
    write(&dir, "history", &history)?;

    // Alternate screen active: two SCREEN and two HISTORY groups.
    let mut alternate = Terminal::new(20, 5)?;
    alternate.vt_write(b"primary\r\n\x1b[?1049halternate");
    write(&dir, "alternate", &alternate)?;

    // Parser mid-sequence: a non-ground CONTINUATION record.
    let mut continuation = Terminal::new(20, 5)?;
    continuation.set_continuation_max_bytes(1 << 20)?;
    continuation.vt_write(b"partial\x1b[3");
    write(&dir, "continuation", &continuation)?;
    Ok(())
}

fn write(
    dir: &std::path::Path,
    name: &str,
    term: &Terminal,
) -> Result<(), Box<dyn std::error::Error>> {
    fs::write(
        dir.join(format!("{name}.ghostsnp")),
        snapshot::encode(term)?,
    )?;
    fs::write(
        dir.join(format!("{name}.txt")),
        formatter::format(term, Options::default())?,
    )?;
    Ok(())
}
