//! Everything Taurus keeps between launches lives in one folder:
//! `~/Library/Application Support/Taurus` on macOS.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use crate::data::{Project, Term};
use crate::engine::Settings;

const LIBRARY: &str = "library.json";
const SETTINGS: &str = "settings.json";

fn app_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("Taurus")
}

/// Debug builds honour `TAURUS_DATA_DIR`, so test runs keep their own library.
pub fn data_dir() -> PathBuf {
    let dir = match std::env::var_os("TAURUS_DATA_DIR") {
        Some(d) if cfg!(debug_assertions) => PathBuf::from(d),
        _ => app_dir(),
    };
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Where a live capture's audio is kept, one WAV per project.
pub fn recording_path(project_id: &str) -> PathBuf {
    data_dir().join("recordings").join(format!("{project_id}.wav"))
}

/// Models are large and shared by every library.
pub fn models_dir() -> PathBuf {
    let dir = app_dir().join("models");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

#[derive(Default, Serialize, Deserialize)]
pub struct Library {
    pub projects: Vec<Project>,
    pub glossary: Vec<Term>,
}

/// Borrowed view of the library, so saving never clones every segment.
#[derive(Serialize)]
pub struct LibraryRef<'a> {
    pub projects: &'a [Project],
    pub glossary: &'a [Term],
}

pub fn load_library() -> Library {
    read_json(&data_dir().join(LIBRARY)).unwrap_or_default()
}

pub fn library_json(lib: &LibraryRef) -> String {
    serde_json::to_string(lib).unwrap_or_default()
}

pub fn write_library(json: &str) -> Result<()> {
    write_atomic(&data_dir().join(LIBRARY), json.as_bytes())
}

pub fn load_settings() -> Settings {
    read_json(&data_dir().join(SETTINGS)).unwrap_or_default()
}

pub fn save_settings(s: &Settings) -> Result<()> {
    let json = serde_json::to_string_pretty(s)?;
    write_atomic(&data_dir().join(SETTINGS), json.as_bytes())
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    let text = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str(&text) {
        Ok(v) => Some(v),
        Err(e) => {
            // A file we cannot read is kept aside rather than silently overwritten.
            let aside = path.with_extension(format!("broken-{}.json", crate::data::now_ms()));
            let _ = std::fs::rename(path, &aside);
            eprintln!("taurus: {} was unreadable ({e}); moved to {}", path.display(), aside.display());
            None
        }
    }
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}
