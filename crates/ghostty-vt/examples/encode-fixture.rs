use std::fs;
use std::path::{Path, PathBuf};

use ghostty_vt::formatter::{self, Options};
use ghostty_vt::{Terminal, snapshot};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = PathBuf::from(
        std::env::args()
            .nth(1)
            .ok_or("usage: encode-fixture <dir>")?,
    );
    fs::create_dir_all(&dir)?;

    let mut basic = Terminal::new(20, 5)?;
    for i in 1..=12 {
        basic.vt_write(format!("line {i} \x1b[1;31mred\x1b[0m\r\n").as_bytes());
    }
    basic.vt_write(b"\x1b]2;fixture\x07cursor here");
    write(&dir, "basic", &basic)?;

    let mut history = Terminal::new(20, 5)?;
    history.set_scrollback_max_bytes(10 * 1024 * 1024)?;
    for i in 1..=5000 {
        history.vt_write(format!("history {i}\r\n").as_bytes());
    }
    write(&dir, "history", &history)?;

    let mut alternate = Terminal::new(20, 5)?;
    alternate.vt_write(b"primary\r\n\x1b[?1049halternate");
    write(&dir, "alternate", &alternate)?;

    let mut continuation = Terminal::new(20, 5)?;
    continuation.set_continuation_max_bytes(1 << 20)?;
    continuation.vt_write(b"partial\x1b[3");
    write(&dir, "continuation", &continuation)?;

    let mut kitty = Terminal::new(20, 5)?;
    kitty.set_scrollback_max_bytes(10 * 1024 * 1024)?;
    for _ in 0..2 {
        kitty.vt_write(&b"line\r\n".repeat(3000));
        kitty.vt_write(b"\x1b_Gf=24,s=1,v=1,a=T,t=d;AAAA\x1b\\");
    }
    kitty.vt_write(&b"line\r\n".repeat(3000));
    write(&dir, "kitty", &kitty)?;
    Ok(())
}

fn write(dir: &Path, name: &str, term: &Terminal) -> Result<(), Box<dyn std::error::Error>> {
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
