//! Everything Taurus keeps between launches lives in one folder:
//! `~/Library/Application Support/Taurus` on macOS. A video project, and a
//! live caption that records the screen, is a folder of its own under
//! `~/Movies/Taurus` instead.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
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
    data_dir()
        .join("recordings")
        .join(format!("{project_id}.wav"))
}

pub fn video_path(project_id: &str) -> PathBuf {
    data_dir()
        .join("recordings")
        .join(format!("{project_id}.mp4"))
}

/// `~/Movies/Taurus`. Debug builds follow `TAURUS_MOVIES_DIR`, or
/// `TAURUS_DATA_DIR/Movies` when that is set, so a test run stays contained.
pub fn movies_dir() -> PathBuf {
    let dir = movies_override().unwrap_or_else(default_movies);
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn default_movies() -> PathBuf {
    dirs::video_dir()
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(std::env::temp_dir)
                .join("Movies")
        })
        .join("Taurus")
}

fn movies_override() -> Option<PathBuf> {
    if !cfg!(any(debug_assertions, test)) {
        return None;
    }
    if let Some(d) = std::env::var_os("TAURUS_MOVIES_DIR") {
        return Some(PathBuf::from(d));
    }
    std::env::var_os("TAURUS_DATA_DIR").map(|d| PathBuf::from(d).join("Movies"))
}

/// A new folder under Movies, named after the project. A name already in use
/// gains a number, so two captures in the same minute do not share a folder.
pub fn project_folder(name: &str) -> Result<PathBuf> {
    unique_folder(&movies_dir(), &folder_name(name))
        .context("could not create a folder in Movies")
}

/// Saves `src` into a new project folder and returns the file there. The
/// original is left where it was. On the same volume this is a clone, so it
/// does not take a second copy of the bytes.
pub fn save_video(project_name: &str, src: &Path) -> Result<PathBuf> {
    let dir = project_folder(project_name)?;
    let dest = dir.join(file_name_of(src));
    if let Err(e) = place_file(src, &dest) {
        let _ = std::fs::remove_file(&dest);
        let _ = std::fs::remove_dir(&dir);
        return Err(e);
    }
    Ok(dest)
}

/// Removes a file Taurus wrote, and the project folder once it is empty.
/// A path outside Movies and the recordings folder is left alone, so deleting
/// a project never deletes a video the user still keeps elsewhere.
pub fn discard_owned(path: &Path) {
    discard_within(&movies_dir(), &data_dir().join("recordings"), path);
}

fn discard_within(movies: &Path, recordings: &Path, path: &Path) {
    if !path.starts_with(movies) && !path.starts_with(recordings) {
        return;
    }
    let _ = std::fs::remove_file(path);
    if let Some(parent) = path.parent()
        && parent.starts_with(movies)
        && parent != movies
        && dir_empty(parent)
    {
        let _ = std::fs::remove_dir(parent);
    }
}

fn unique_folder(root: &Path, name: &str) -> Result<PathBuf> {
    std::fs::create_dir_all(root)?;
    for n in 1..10_000 {
        let dir = if n == 1 {
            root.join(name)
        } else {
            root.join(format!("{name} {n}"))
        };
        match std::fs::create_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).context(format!("creating {}", dir.display())),
        }
    }
    bail!("too many folders named {name}")
}

fn folder_name(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if c == '/' || c == ':' || c == '\\' || c == '\0' || c.is_control() {
            out.push(' ');
        } else {
            out.push(c);
        }
    }
    let out: String = out.split_whitespace().collect::<Vec<_>>().join(" ");
    let out = out
        .trim_matches(|c: char| c == '.' || c == ' ')
        .chars()
        .take(80)
        .collect::<String>();
    if out.is_empty() {
        "Video".into()
    } else {
        out
    }
}

fn file_name_of(path: &Path) -> String {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains(':') {
        "video.mp4".into()
    } else {
        name
    }
}

fn place_file(src: &Path, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let cloned = std::process::Command::new("/bin/cp")
        .arg("-c")
        .arg(src)
        .arg(dest)
        .status()
        .is_ok_and(|s| s.success());
    if cloned && dest.is_file() {
        return Ok(());
    }
    let _ = std::fs::remove_file(dest);
    std::fs::copy(src, dest)
        .with_context(|| format!("saving {} into {}", src.display(), dest.display()))?;
    Ok(())
}

fn dir_empty(path: &Path) -> bool {
    std::fs::read_dir(path)
        .map(|rd| rd.flatten().next().is_none())
        .unwrap_or(false)
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
            eprintln!(
                "taurus: {} was unreadable ({e}); moved to {}",
                path.display(),
                aside.display()
            );
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_names_are_safe_and_short() {
        assert_eq!(folder_name("night market"), "night market");
        assert_eq!(folder_name("a/b:c"), "a b c");
        assert_eq!(folder_name("  ..  "), "Video");
        assert_eq!(folder_name(&"n".repeat(200)).chars().count(), 80);
    }

    #[test]
    fn a_project_is_its_own_folder() {
        let root = std::env::temp_dir().join(format!(
            "taurus-folders-{}-{}",
            std::process::id(),
            crate::data::now_ms()
        ));
        let a = unique_folder(&root, "Talk").unwrap();
        let b = unique_folder(&root, "Talk").unwrap();
        assert_ne!(a, b);
        assert!(a.is_dir() && b.is_dir());
        assert_eq!(a.file_name().unwrap(), "Talk");
        assert_eq!(b.file_name().unwrap(), "Talk 2");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_video_lands_in_the_folder_and_the_original_stays() {
        let scratch = std::env::temp_dir().join(format!(
            "taurus-movies-{}-{}",
            std::process::id(),
            crate::data::now_ms()
        ));
        let movies = scratch.join("movies");
        let src = scratch.join("kitchen.mp4");
        std::fs::create_dir_all(&scratch).unwrap();
        std::fs::write(&src, b"video-bytes").unwrap();

        let dir = unique_folder(&movies, &folder_name("kitchen: e03")).unwrap();
        let saved = dir.join(file_name_of(&src));
        place_file(&src, &saved).unwrap();
        assert_eq!(saved, movies.join("kitchen e03").join("kitchen.mp4"));
        assert_eq!(std::fs::read(&saved).unwrap(), b"video-bytes");
        assert_eq!(std::fs::read(&src).unwrap(), b"video-bytes");

        let outside = scratch.join("keep.mp4");
        std::fs::write(&outside, b"keep").unwrap();
        discard_within(&movies, &scratch.join("recordings"), &outside);
        assert!(outside.is_file());

        discard_within(&movies, &scratch.join("recordings"), &saved);
        assert!(!saved.exists());
        assert!(!saved.parent().unwrap().exists());
        let _ = std::fs::remove_dir_all(&scratch);
    }
}
