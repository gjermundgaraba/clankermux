//! Writes snapshot fixtures for the cross-engine test: each `<name>.ghostsnp`
//! is a native-encoded snapshot and `<name>.txt` the plain-text screen the
//! native formatter produces for it.

use std::fs;
use std::path::PathBuf;

use ghostty_vt::Terminal;
use ghostty_vt::formatter::{self, Options};
use ghostty_vt::snapshot;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = PathBuf::from(
        std::env::args()
            .nth(1)
            .ok_or("usage: encode-fixture <dir>")?,
    );
    fs::create_dir_all(&dir)?;
    let mut term = Terminal::new(20, 5)?;
    for i in 1..=12 {
        term.vt_write(format!("line {i} \x1b[1;31mred\x1b[0m\r\n").as_bytes());
    }
    term.vt_write(b"\x1b]2;fixture\x07cursor here");
    fs::write(dir.join("basic.ghostsnp"), snapshot::encode(&term)?)?;
    fs::write(
        dir.join("basic.txt"),
        formatter::format(&term, Options::default())?,
    )?;
    Ok(())
}
