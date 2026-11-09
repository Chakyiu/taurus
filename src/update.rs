//! New versions come from the Taurus releases on GitHub. Taurus finds out
//! whether there is one, then downloads it, checks it and puts it in place of
//! the running app, which carries on as it was until it restarts.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, bail, ensure};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::engine::{agent, check, fetch};

pub const RELEASES: &str = "https://github.com/Chakyiu/taurus/releases";
pub const REPO: &str = "https://github.com/Chakyiu/taurus";
const LATEST: &str = "https://api.github.com/repos/Chakyiu/taurus/releases/latest";
const BUNDLE_ID: &str = "app.taurus.workbench";
/// Where an update is unpacked: beside the app, so moving it in is a rename.
const STAGING: &str = ".taurus-update";
pub const DAY: i64 = 24 * 60 * 60;

/// Set by the release workflow. Only its builds update themselves unasked;
/// one built from source keeps its own version until told otherwise.
pub const AUTOMATIC: bool = option_env!("TAURUS_RELEASE").is_some();

/// This build's version. The release workflow stamps it from the tag.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// Debug builds honour `TAURUS_UPDATE_URL`, to try updates against a stand-in.
pub fn latest_url() -> String {
    match std::env::var("TAURUS_UPDATE_URL") {
        Ok(url) if cfg!(debug_assertions) => url,
        _ => LATEST.into(),
    }
}

/// A release newer than this build.
#[derive(Clone, Debug, PartialEq)]
pub struct Release {
    pub version: String,
    /// Its page on GitHub: the notes, and the files to download by hand.
    pub page: String,
    /// The zipped app, by name and address.
    zip: Option<(String, String)>,
    /// The published checksums of the release's files.
    sums: Option<String>,
}

impl Release {
    /// Whether it has what installing needs: the app and its checksum.
    pub fn installable(&self) -> bool {
        self.zip.is_some() && self.sums.is_some()
    }
}

/// The latest release at `url`, if it is newer than `current`.
pub fn newer_release(url: &str, current: &str) -> Result<Option<Release>> {
    let resp = agent(Duration::from_secs(20))
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", format!("Taurus/{}", version()))
        .call()
        .context("contacting GitHub")?;
    // GitHub answers 404 until there is a first release.
    if resp.status() == 404 {
        return Ok(None);
    }
    let body = check(resp)?.body_mut().read_to_string().context("reading the release")?;
    parse_release(&body, current)
}

fn parse_release(body: &str, current: &str) -> Result<Option<Release>> {
    let v: Value = serde_json::from_str(body).context("reading the release")?;
    let tag = v["tag_name"].as_str().context("the release has no version")?;
    let version = tag.trim_start_matches('v').to_string();
    let unfinished = v["draft"].as_bool() == Some(true) || v["prerelease"].as_bool() == Some(true);
    if unfinished || !is_newer(&version, current) {
        return Ok(None);
    }
    let zip_suffix = format!("-macos-{}.zip", arch());
    let assets = v["assets"].as_array().map(Vec::as_slice).unwrap_or_default();
    let find = |want: &dyn Fn(&str) -> bool| {
        assets.iter().find_map(|a| {
            let name = a["name"].as_str()?;
            let url = a["browser_download_url"].as_str()?;
            want(name).then(|| (name.to_string(), url.to_string()))
        })
    };
    Ok(Some(Release {
        page: v["html_url"].as_str().unwrap_or(RELEASES).to_string(),
        zip: find(&|n| n.ends_with(&zip_suffix)),
        sums: find(&|n| n == "SHA256SUMS.txt").map(|(_, url)| url),
        version,
    }))
}

/// How the release workflow names this Mac's architecture.
fn arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        other => other,
    }
}

/// Whether `candidate` is a later version than `current`. A pre-release
/// comes before the release of the same number.
fn is_newer(candidate: &str, current: &str) -> bool {
    fn parse(v: &str) -> Option<([u64; 3], bool)> {
        let (number, pre) = match v.split_once('-') {
            Some((number, _)) => (number, true),
            None => (v, false),
        };
        let mut parts = number.split('.').map(|p| p.parse::<u64>().ok());
        let triple = [parts.next()??, parts.next()??, parts.next()??];
        parts.next().is_none().then_some((triple, pre))
    }
    match (parse(candidate), parse(current)) {
        (Some((new, new_pre)), Some((old, old_pre))) => new > old || (new == old && old_pre && !new_pre),
        _ => false,
    }
}

/// The app bundle at `app_path`, unless Taurus runs from a development build.
pub fn bundle(app_path: &Path) -> Option<PathBuf> {
    app_path.extension().is_some_and(|e| e == "app").then(|| app_path.to_path_buf())
}

/// Whether the folder holding `bundle` takes a new app. A disk image, or an
/// app macOS runs from a hidden copy until it is moved, does not.
pub fn can_replace(bundle: &Path) -> bool {
    let Some(parent) = bundle.parent() else { return false };
    let probe = parent.join(format!("{STAGING}-probe-{}", std::process::id()));
    let made = std::fs::create_dir(&probe).is_ok();
    let _ = std::fs::remove_dir(&probe);
    made
}

/// Downloads `release`, checks it against its published checksum, and puts
/// it in place of `bundle`.
pub fn install(release: &Release, bundle: &Path, progress: impl Fn(f32), cancel: &AtomicBool) -> Result<()> {
    let (Some((zip_name, zip_url)), Some(sums_url)) = (&release.zip, &release.sums) else {
        bail!("this release has no app to install");
    };
    let staging = bundle.parent().context("the app has no folder")?.join(STAGING);
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir(&staging).context("making room for the update")?;
    let result = (|| {
        let zip = staging.join(zip_name);
        let mut on_bytes = |done: u64, total: u64| {
            if total > 0 {
                progress(done as f32 / total as f32);
            }
        };
        fetch(zip_url, &zip, &mut on_bytes, cancel)?;
        let sums = read_text(sums_url).context("fetching the checksums")?;
        let want = expected_sum(&sums, zip_name).context("the checksums do not list the app")?;
        ensure!(sha256_file(&zip)? == want, "the download does not match its checksum");
        run(Command::new("/usr/bin/ditto").args(["-x", "-k"]).arg(&zip).arg(&staging))?;
        let app = std::fs::read_dir(&staging)?
            .filter_map(|e| Some(e.ok()?.path()))
            .find(|p| p.extension().is_some_and(|e| e == "app"))
            .context("the download has no app in it")?;
        check_app(&app, &release.version)?;
        if cancel.load(Ordering::Relaxed) {
            bail!("cancelled");
        }
        swap(bundle, &app)
    })();
    let _ = std::fs::remove_dir_all(&staging);
    result
}

/// The unpacked app has to be Taurus, the version promised, and intact.
fn check_app(app: &Path, version: &str) -> Result<()> {
    let plist = app.join("Contents/Info.plist");
    ensure!(plist_value(&plist, "CFBundleIdentifier")? == BUNDLE_ID, "the download is not Taurus");
    let found = plist_value(&plist, "CFBundleShortVersionString")?;
    ensure!(found == version, "the download is version {found}, not {version}");
    run(Command::new("/usr/bin/codesign").args(["--verify", "--deep", "--strict"]).arg(app))
}

/// The old app is renamed within its own folder rather than moved into the
/// staging one, which would also need write access to the app itself.
fn swap(bundle: &Path, new_app: &Path) -> Result<()> {
    let name = bundle.file_name().context("the app has no name")?.to_string_lossy();
    let old = bundle.with_file_name(format!(".{name}-old"));
    let _ = std::fs::remove_dir_all(&old);
    std::fs::rename(bundle, &old).context("moving the old app aside")?;
    if let Err(e) = std::fs::rename(new_app, bundle) {
        let _ = std::fs::rename(&old, bundle);
        return Err(anyhow::Error::new(e).context("moving the new app in"));
    }
    let _ = std::fs::remove_dir_all(&old);
    Ok(())
}

fn plist_value(plist: &Path, key: &str) -> Result<String> {
    let out = Command::new("/usr/libexec/PlistBuddy")
        .arg("-c")
        .arg(format!("Print :{key}"))
        .arg(plist)
        .output()
        .context("reading the app's details")?;
    ensure!(out.status.success(), "the app has no {key}");
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn run(cmd: &mut Command) -> Result<()> {
    let name = Path::new(cmd.get_program()).file_name().unwrap_or_default().to_string_lossy().to_string();
    let out = cmd.output().with_context(|| format!("running {name}"))?;
    if !out.status.success() {
        bail!("{name} failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

fn read_text(url: &str) -> Result<String> {
    let resp = agent(Duration::from_secs(30)).get(url).call()?;
    Ok(check(resp)?.body_mut().read_to_string()?)
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut hasher = Sha256::new();
    std::io::copy(&mut std::fs::File::open(path)?, &mut hasher)?;
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// The checksum `shasum` wrote for `name`.
fn expected_sum(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let (sum, file) = line.split_once(char::is_whitespace)?;
        (file.trim().trim_start_matches('*') == name).then(|| sum.to_ascii_lowercase())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions() {
        assert!(is_newer("0.1.3", "0.1.2"));
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(is_newer("0.10.0", "0.9.0"));
        assert!(is_newer("1.0.0", "0.9.12"));
        assert!(is_newer("0.2.0", "0.2.0-beta.1"));
        assert!(!is_newer("0.1.2", "0.1.2"));
        assert!(!is_newer("0.1.1", "0.1.2"));
        assert!(!is_newer("0.2.0-beta.1", "0.2.0"));
        assert!(!is_newer("nightly", "0.1.2"));
        assert!(!is_newer("0.1", "0.0.9"));
    }

    #[test]
    fn reads_the_latest_release() {
        let release = |tag: &str, prerelease: bool| {
            serde_json::json!({
                "tag_name": tag,
                "html_url": format!("{RELEASES}/tag/{tag}"),
                "draft": false,
                "prerelease": prerelease,
                "assets": [
                    { "name": "SHA256SUMS.txt", "browser_download_url": "https://x/SHA256SUMS.txt" },
                    { "name": format!("Taurus-{tag}-macos-{}.dmg", arch()), "browser_download_url": "https://x/t.dmg" },
                    { "name": format!("Taurus-{tag}-macos-{}.zip", arch()), "browser_download_url": "https://x/t.zip" },
                ],
            })
            .to_string()
        };
        let r = parse_release(&release("v0.1.3", false), "0.1.2").unwrap().unwrap();
        assert_eq!(r.version, "0.1.3");
        assert_eq!(r.page, format!("{RELEASES}/tag/v0.1.3"));
        assert_eq!(r.zip.as_ref().map(|(_, url)| url.as_str()), Some("https://x/t.zip"));
        assert!(r.installable());
        assert_eq!(parse_release(&release("v0.1.2", false), "0.1.2").unwrap(), None);
        assert_eq!(parse_release(&release("v0.2.0-beta.1", true), "0.1.2").unwrap(), None);
        let bare = r#"{"tag_name":"v0.1.3","html_url":"https://x","assets":[]}"#;
        assert!(!parse_release(bare, "0.1.2").unwrap().unwrap().installable());
    }

    #[test]
    fn finds_the_checksum() {
        let sums = "9d31  Taurus-v0.1.2-macos-arm64.zip\n4cdb  Taurus-v0.1.2-macos-arm64.dmg\n";
        assert_eq!(expected_sum(sums, "Taurus-v0.1.2-macos-arm64.dmg").as_deref(), Some("4cdb"));
        assert_eq!(expected_sum("ABCD *t.zip", "t.zip").as_deref(), Some("abcd"));
        assert_eq!(expected_sum(sums, "t.zip"), None);
    }
}
