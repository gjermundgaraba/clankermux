//! Parked sessions on disk: `sessions/<id>/<generation>/` with
//! `session.json`, `snapshot.ghostsnp` and `screen.txt`.

use std::fs::{self, File};
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use proto::Profile;
use serde::{Deserialize, Serialize};

pub const PROTOCOL: u32 = 1;

/// The cell size in pixels last reported, 0 when unknown.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cell {
    pub width: u32,
    pub height: u32,
}

/// `session.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParkFile {
    pub protocol: u32,
    pub engine: String,
    pub parked_at: String,
    /// The session as `inspect` reports it, records included.
    pub session: proto::Session,
    pub profile: Profile,
    pub cell: Cell,
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
        sync_dir(&dir)
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
/// three files; the others and any dot-directory are deleted, as is a
/// session directory without a complete generation.
pub fn load(sessions: &Path) -> io::Result<Vec<Loaded>> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(sessions) else {
        return Ok(out);
    };
    for session_dir in entries.flatten().map(|e| e.path()).filter(|p| p.is_dir()) {
        let mut generations: Vec<(u64, PathBuf)> = Vec::new();
        for entry in fs::read_dir(&session_dir)?.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            match name.parse::<u64>() {
                Ok(generation)
                    if !name.starts_with('.') && FILES.iter().all(|f| path.join(f).is_file()) =>
                {
                    generations.push((generation, path));
                }
                _ => {
                    let _ = fs::remove_dir_all(&path);
                }
            }
        }
        generations.sort();
        let Some((_, newest)) = generations.pop() else {
            let _ = fs::remove_dir_all(&session_dir);
            continue;
        };
        for (_, older) in generations {
            let _ = fs::remove_dir_all(older);
        }
        let file: ParkFile = match fs::read(newest.join(FILES[0]))
            .and_then(|b| serde_json::from_slice(&b).map_err(io::Error::other))
        {
            Ok(file) => file,
            Err(err) => {
                tracing::warn!(path = %newest.display(), error = %err, "unreadable park file");
                continue;
            }
        };
        if file.protocol != PROTOCOL {
            tracing::warn!(path = %newest.display(), protocol = file.protocol, "unknown park protocol");
            continue;
        }
        let screen = fs::read_to_string(newest.join(FILES[2]))?;
        out.push(Loaded {
            file,
            screen,
            snapshot_path: newest.join(FILES[1]),
        });
    }
    Ok(out)
}

/// Deletes everything parked for `id`.
pub fn remove(sessions: &Path, id: &str) {
    let _ = fs::remove_dir_all(sessions.join(id));
}
