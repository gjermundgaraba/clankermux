//! Parked sessions on disk: `sessions/<id>/<generation>/` with
//! `session.json`, `snapshot.ghostsnp` and `screen.txt`.

use std::fs::{self, File};
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const PROTOCOL: u32 = 1;

/// `session.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParkFile {
    pub protocol: u32,
    pub engine: String,
    pub parked_at: String,
    /// The session as `inspect` reports it, records included.
    pub session: proto::Session,
    /// How many leading lines of `screen.txt` are history, the rest being
    /// the screen; `read` takes `scrollback` from them.
    pub history_lines: u64,
}

/// A session's newest complete park, as loaded at start.
pub struct Loaded {
    pub file: ParkFile,
    pub screen: String,
    pub snapshot_path: PathBuf,
}

const FILES: [&str; 3] = ["session.json", "snapshot.ghostsnp", "screen.txt"];

fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

fn write_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use io::Write as _;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// Writes one generation for `file.session.id` under `sessions`: into a
/// dot-directory first, each file and the directory synced, then renamed
/// to `generation`; older generations are deleted after.
pub fn write(
    sessions: &Path,
    file: &ParkFile,
    generation: u64,
    snapshot: &[u8],
    screen: &str,
) -> io::Result<()> {
    let dir = sessions.join(&file.session.id);
    let first = !dir.is_dir();
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)?;
    let temp = dir.join(format!(".{generation}"));
    let _ = fs::remove_dir_all(&temp);
    fs::DirBuilder::new().mode(0o700).create(&temp)?;
    let result = (|| {
        write_file(&temp.join(FILES[0]), &serde_json::to_vec(file)?)?;
        write_file(&temp.join(FILES[1]), snapshot)?;
        write_file(&temp.join(FILES[2]), screen.as_bytes())?;
        sync_dir(&temp)?;
        fs::rename(&temp, dir.join(generation.to_string()))?;
        sync_dir(&dir)?;
        // The session's own directory entry is new too.
        if first { sync_dir(sessions) } else { Ok(()) }
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&temp);
        return result;
    }
    for entry in fs::read_dir(&dir)?.flatten() {
        let name = entry.file_name();
        if name
            .to_string_lossy()
            .parse::<u64>()
            .is_ok_and(|g| g < generation)
        {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
    Ok(())
}

/// Loads every session under `sessions`: the newest generation holding all
/// three files and a readable `session.json`; the others and any
/// dot-directory are deleted, as is a session directory left with
/// nothing. A session that cannot be read is logged and skipped, never a
/// reason not to start.
pub fn load(sessions: &Path) -> Vec<Loaded> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(sessions) else {
        return out;
    };
    for session_dir in entries.flatten().map(|e| e.path()).filter(|p| p.is_dir()) {
        match load_session(&session_dir) {
            Ok(Some(loaded)) => out.push(loaded),
            Ok(None) => {
                let _ = fs::remove_dir_all(&session_dir);
            }
            Err(err) => {
                tracing::warn!(path = %session_dir.display(), error = %err, "park directory skipped");
            }
        }
    }
    out
}

/// The newest readable generation of one session, `None` when there is
/// none left. A file naming another session is an error: its id would
/// be removed under a path of its choosing.
fn load_session(session_dir: &Path) -> io::Result<Option<Loaded>> {
    let mut generations: Vec<(u64, PathBuf)> = Vec::new();
    for entry in fs::read_dir(session_dir)?.flatten() {
        let path = entry.path();
        // A dot-directory is an unfinished park; its name does not parse.
        match entry.file_name().to_string_lossy().parse::<u64>() {
            Ok(generation) if FILES.iter().all(|f| path.join(f).is_file()) => {
                generations.push((generation, path));
            }
            _ => {
                let _ = fs::remove_dir_all(&path);
            }
        }
    }
    generations.sort();
    let mut chosen = None;
    while let Some((_, newest)) = generations.pop() {
        match read_generation(&newest) {
            Ok(loaded) => {
                chosen = Some(loaded);
                break;
            }
            Err(err) => {
                tracing::warn!(path = %newest.display(), error = %err, "unreadable park; deleted");
                let _ = fs::remove_dir_all(&newest);
            }
        }
    }
    for (_, older) in generations {
        let _ = fs::remove_dir_all(older);
    }
    if let Some(loaded) = &chosen
        && Some(loaded.file.session.id.as_str()) != session_dir.file_name().and_then(|n| n.to_str())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("park names session {}", loaded.file.session.id),
        ));
    }
    Ok(chosen)
}

fn read_generation(dir: &Path) -> io::Result<Loaded> {
    let file: ParkFile = serde_json::from_slice(&fs::read(dir.join(FILES[0]))?)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if file.protocol != PROTOCOL {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("park protocol {}", file.protocol),
        ));
    }
    Ok(Loaded {
        file,
        screen: fs::read_to_string(dir.join(FILES[2]))?,
        snapshot_path: dir.join(FILES[1]),
    })
}

/// Replaces the records in the `session.json` of the generation that
/// holds `snapshot`, by a rename, keeping the rest of the file.
pub fn rewrite_records(snapshot: &Path, records: Vec<proto::StatusRecord>) -> io::Result<()> {
    let dir = snapshot
        .parent()
        .ok_or_else(|| io::Error::other("no generation directory"))?;
    let path = dir.join(FILES[0]);
    let mut file: ParkFile = serde_json::from_slice(&fs::read(&path)?)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    file.session.program_status = Some(records);
    let temp = dir.join(format!(".{}", FILES[0]));
    let _ = fs::remove_file(&temp);
    write_file(&temp, &serde_json::to_vec(&file)?)?;
    fs::rename(&temp, &path)?;
    sync_dir(dir)
}

/// Deletes everything parked for `id`.
pub fn remove(sessions: &Path, id: &str) {
    let _ = fs::remove_dir_all(sessions.join(id));
}
