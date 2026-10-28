use std::fs::{self, File};
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParkFile {
    pub version: u32,
    pub engine: String,
    pub parked_at: String,
    pub session: proto::Session,
    pub history_lines: usize,
    pub commands: Vec<proto::Command>,
}

pub struct Loaded {
    pub file: ParkFile,
    pub screen: String,
    pub snapshot_path: PathBuf,
}

const SESSION_JSON: &str = "session.json";
const SNAPSHOT: &str = "snapshot.ghostsnp";
const SCREEN: &str = "screen.txt";
const FILES: [&str; 3] = [SESSION_JSON, SNAPSHOT, SCREEN];

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
        write_file(&temp.join(SESSION_JSON), &serde_json::to_vec(file)?)?;
        write_file(&temp.join(SNAPSHOT), snapshot)?;
        write_file(&temp.join(SCREEN), screen.as_bytes())?;
        sync_dir(&temp)?;
        fs::rename(&temp, dir.join(generation.to_string()))?;
        sync_dir(&dir)?;
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

fn load_session(session_dir: &Path) -> io::Result<Option<Loaded>> {
    let mut generations: Vec<(u64, PathBuf)> = Vec::new();
    for entry in fs::read_dir(session_dir)?.flatten() {
        let path = entry.path();
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

fn read_session_json(path: &Path) -> io::Result<ParkFile> {
    serde_json::from_slice(&fs::read(path)?)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn read_generation(dir: &Path) -> io::Result<Loaded> {
    let file = read_session_json(&dir.join(SESSION_JSON))?;
    if file.version != VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("park version {}", file.version),
        ));
    }
    Ok(Loaded {
        file,
        screen: fs::read_to_string(dir.join(SCREEN))?,
        snapshot_path: dir.join(SNAPSHOT),
    })
}

pub fn rewrite_records(snapshot: &Path, records: Vec<proto::StatusRecord>) -> io::Result<()> {
    let dir = snapshot
        .parent()
        .ok_or_else(|| io::Error::other("no generation directory"))?;
    let path = dir.join(SESSION_JSON);
    let mut file = read_session_json(&path)?;
    file.session.program_status = Some(records);
    let temp = dir.join(format!(".{SESSION_JSON}"));
    let _ = fs::remove_file(&temp);
    write_file(&temp, &serde_json::to_vec(&file)?)?;
    fs::rename(&temp, &path)?;
    sync_dir(dir)
}

pub fn remove(sessions: &Path, id: &str) {
    let _ = fs::remove_dir_all(sessions.join(id));
}
