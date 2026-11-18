//! The daily update check: one unauthenticated `GET` of the latest release, at most once per 24 h, never when `update.check = false`.
//! Yields a `Release` only for a newer `MAJOR.MINOR.PATCH`; every failure is one INFO line. The check installs nothing.
//! `byovox update` is the only install: `stage` downloads and verifies the latest release beside the running exe, `swap` puts it in place.

use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::deploy;

pub const LATEST_URL: &str = "https://api.github.com/repos/iteramen/byovox/releases/latest";

/// Where a release's assets are fetched from: built from the tag, never read from the API reply.
pub fn release_download(tag: &str, name: &str) -> String {
    format!("https://github.com/iteramen/byovox/releases/download/{tag}/{name}")
}

const DAY: u64 = 24 * 60 * 60;

const TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    pub url: String,
    /// True the first time this version is seen; the stamp remembers it after that.
    pub balloon: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Stamp {
    checked: u64,
    notified: String,
}

#[derive(Deserialize)]
struct Latest {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    size: u64,
}

/// The pair `byovox update` replaces, named as in the release zip and in the folder it was unzipped to.
pub const EXES: [&str; 2] = ["byovox.exe", "byovox-daemon.exe"];

const CARGO_INSTALL: &str = "cargo install --git https://github.com/iteramen/byovox --locked";

pub fn stamp_path() -> PathBuf {
    crate::config::data_dir().join("update.json")
}

/// Runs the check on a thread of its own and returns at once; `None` when `enabled` is false.
pub fn spawn<F>(enabled: bool, url: &str, stamp: PathBuf, on_newer: F) -> Option<JoinHandle<()>>
where
    F: FnOnce(Release) + Send + 'static,
{
    if !enabled {
        return None;
    }
    let url = url.to_string();
    std::thread::Builder::new()
        .name("byovox-update".into())
        .spawn(move || {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            if let Some(release) = check(
                &url,
                &stamp,
                now,
                env!("CARGO_PKG_VERSION"),
                TIMEOUT,
                CONNECT_TIMEOUT,
            ) {
                on_newer(release);
            }
        })
        .inspect_err(|e| tracing::info!(error = %e, "update check not started"))
        .ok()
}

/// A stamp that cannot be written skips the request, so no failure can turn into a request per start.
fn check(
    url: &str,
    stamp_path: &Path,
    now: u64,
    current: &str,
    timeout: Duration,
    connect_timeout: Duration,
) -> Option<Release> {
    let mut stamp: Stamp = std::fs::read(stamp_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    // Either side of the stamp: a clock stepped back must neither cause a second request nor hold the check off for long.
    if now.abs_diff(stamp.checked) < DAY {
        tracing::debug!("update check skipped: the last one was under a day ago");
        return None;
    }
    stamp.checked = now;
    if let Err(e) = write_stamp(stamp_path, &stamp) {
        tracing::info!(error = %e, "update check skipped: the stamp file cannot be written");
        return None;
    }
    let latest = match fetch(url, current, timeout, connect_timeout) {
        Ok(latest) => latest,
        Err(e) => {
            tracing::info!(error = %e, "update check failed; the next one is in a day");
            return None;
        }
    };
    let version = latest
        .tag_name
        .strip_prefix('v')
        .unwrap_or(&latest.tag_name);
    if latest.prerelease || !is_newer(version, current) {
        tracing::debug!(tag = %latest.tag_name, "no newer byovox release");
        return None;
    }
    let balloon = stamp.notified != version;
    if balloon {
        stamp.notified = version.to_string();
        if let Err(e) = write_stamp(stamp_path, &stamp) {
            tracing::debug!(error = %e, "the announced version was not recorded");
        }
    }
    tracing::info!(version, "a newer byovox is available");
    Some(Release {
        version: version.to_string(),
        url: latest.html_url,
        balloon,
    })
}

fn write_stamp(path: &Path, stamp: &Stamp) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, serde_json::to_vec(stamp)?)
}

fn fetch(
    url: &str,
    current: &str,
    timeout: Duration,
    connect_timeout: Duration,
) -> Result<Latest, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(connect_timeout))
        .timeout_global(Some(timeout))
        .user_agent(format!("byovox/{current}"))
        .build()
        .into();
    latest(&agent, url)
}

/// A page that is not on github.com is refused: the tray hands it to `explorer`.
fn latest(agent: &ureq::Agent, url: &str) -> Result<Latest, String> {
    let body = agent
        .get(url)
        .call()
        .map_err(|e| e.to_string())?
        .body_mut()
        .read_to_string()
        .map_err(|e| e.to_string())?;
    let latest: Latest = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    if !latest.html_url.starts_with("https://github.com/") {
        return Err(format!(
            "release page is not on github.com: {}",
            latest.html_url
        ));
    }
    Ok(latest)
}

/// Downloads the latest release's zip and `SHA256SUMS` from the release `url` describes, and unpacks
/// the verified pair into `dir` as `<name>.new`. Returns the release's version. Every refusal comes
/// before anything is written to `dir`.
pub fn stage(
    agent: &ureq::Agent,
    url: &str,
    download: &dyn Fn(&str, &str) -> String,
    current: &str,
    dir: &Path,
    scratch: &Path,
) -> Result<String, String> {
    // cargo keeps these beside the `bin` folder of every root it installs into.
    if [".crates.toml", ".crates2.json"]
        .iter()
        .any(|m| dir.parent().is_some_and(|root| root.join(m).is_file()))
    {
        return Err(format!(
            "this byovox was installed by cargo; update it with: {CARGO_INSTALL}"
        ));
    }
    if let Some(missing) = EXES.iter().find(|n| !dir.join(n).is_file()) {
        return Err(format!(
            "{missing} is not in {}: byovox update replaces the pair unzipped from a release",
            dir.display()
        ));
    }
    let latest = latest(agent, url)?;
    let tag = &latest.tag_name;
    let version = tag.strip_prefix('v').unwrap_or(tag);
    if latest.prerelease || !is_newer(version, current) {
        return Err(format!(
            "byovox {current} is up to date: the latest release is {tag}"
        ));
    }
    let zip_name = format!("byovox-{tag}-x86_64-pc-windows-msvc.zip");
    let asset = |name: &str| {
        latest
            .assets
            .iter()
            .find(|a| a.name == name)
            .ok_or_else(|| format!("release {tag} has no {name}"))
    };
    let zip = asset(&zip_name)?;
    asset("SHA256SUMS")?;
    let sums = agent
        .get(download(tag, "SHA256SUMS"))
        .call()
        .and_then(|mut r| r.body_mut().read_to_string())
        .map_err(|e| format!("SHA256SUMS: {e}"))?;
    let sha256 = sha256_for(&sums, &zip_name)
        .ok_or_else(|| format!("SHA256SUMS has no checksum for {zip_name}"))?;
    // Leaked into a `Pin`, whose fields are 'static; the command exits after one update.
    let pin = deploy::Pin {
        url: download(tag, &zip_name).leak(),
        bytes: zip.size,
        sha256: sha256.leak(),
    };
    let path = deploy::fetch(agent, &pin, scratch)?;
    let unpacked = unpack(&path, dir);
    let _ = std::fs::remove_file(&path);
    unpacked?;
    Ok(version.to_string())
}

/// The hash on the `sha256sum` line naming `file`: 64 hex digits, a space, then a space or `*`.
fn sha256_for(sums: &str, file: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let (hash, rest) = line.split_once(' ')?;
        let name = rest.strip_prefix([' ', '*'])?;
        (name.trim_end() == file && hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| hash.to_ascii_lowercase())
    })
}

fn unpack(zip: &Path, dir: &Path) -> Result<(), String> {
    let failed = |e: &dyn std::fmt::Display| format!("unpacking {}: {e}", zip.display());
    let file = std::fs::File::open(zip).map_err(|e| failed(&e))?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| failed(&e))?;
    if let Some(missing) = EXES.iter().find(|n| archive.index_for_name(n).is_none()) {
        return Err(failed(&format!("it has no {missing}")));
    }
    for name in EXES {
        let mut entry = archive.by_name(name).map_err(|e| failed(&e))?;
        let mut out =
            std::fs::File::create(dir.join(format!("{name}.new"))).map_err(|e| failed(&e))?;
        std::io::copy(&mut entry, &mut out).map_err(|e| failed(&e))?;
    }
    Ok(())
}

/// Renames each exe in `dir` to `<name>.old` and its staged `<name>.new` into its place: a running
/// exe can be renamed but not overwritten. A failure puts back every exe already moved.
pub fn swap(dir: &Path) -> Result<(), String> {
    let mut moved = Vec::new();
    for name in EXES {
        let exe = dir.join(name);
        let old = dir.join(format!("{name}.old"));
        let placed = std::fs::rename(&exe, &old)
            .map(|()| moved.push((exe.clone(), old)))
            .and_then(|()| std::fs::rename(dir.join(format!("{name}.new")), &exe));
        if let Err(e) = placed {
            let mut error = format!("replacing {name}: {e}");
            for (exe, old) in moved.iter().rev() {
                if let Err(e) = std::fs::rename(old, exe) {
                    error.push_str(&format!(
                        "; putting {} back failed ({e}): the previous one is {}",
                        exe.display(),
                        old.display()
                    ));
                }
            }
            return Err(error);
        }
    }
    Ok(())
}

/// Anything but a plain newer `MAJOR.MINOR.PATCH` (pre-release, build metadata, extra part, leading zero) is never newer.
fn is_newer(candidate: &str, current: &str) -> bool {
    match (triple(candidate), triple(current)) {
        (Some(c), Some(r)) => c > r,
        _ => false,
    }
}

fn triple(v: &str) -> Option<(u64, u64, u64)> {
    let mut parts = v.split('.').map(|p| {
        let leading_zero = p.len() > 1 && p.starts_with('0');
        if !p.is_empty() && !leading_zero && p.bytes().all(|b| b.is_ascii_digit()) {
            p.parse::<u64>().ok()
        } else {
            None
        }
    });
    let t = (parts.next()??, parts.next()??, parts.next()??);
    parts.next().is_none().then_some(t)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{MockServer, logged};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::time::Instant;

    const T0: u64 = 1_790_000_000;

    fn release(tag: &str) -> String {
        format!(
            r#"{{"tag_name":"{tag}","html_url":"https://github.com/iteramen/byovox/releases/tag/{tag}","prerelease":false,"assets":[]}}"#
        )
    }

    fn temp_dir(test: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("byovox-update-{}-{test}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn run(url: &str, stamp: &Path, now: u64) -> Option<Release> {
        check(
            url,
            stamp,
            now,
            "0.3.0",
            Duration::from_secs(5),
            Duration::from_secs(5),
        )
    }

    #[test]
    fn a_newer_release_is_announced_with_its_page() {
        let dir = temp_dir("newer");
        let srv = MockServer::start(200, &release("v0.4.0"));
        let got = run(srv.url(), &dir.join("update.json"), T0);
        assert_eq!(
            got,
            Some(Release {
                version: "0.4.0".into(),
                url: "https://github.com/iteramen/byovox/releases/tag/v0.4.0".into(),
                balloon: true,
            })
        );
        let srv = MockServer::start(200, &release("v0.10.0"));
        let got = run(srv.url(), &dir.join("second.json"), T0);
        assert_eq!(got.map(|r| r.version).as_deref(), Some("0.10.0"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_request_carries_only_the_version_user_agent() {
        let dir = temp_dir("user_agent");
        let srv = MockServer::start(200, &release("v0.3.0"));
        run(srv.url(), &dir.join("update.json"), T0);
        let reqs = srv.requests();
        assert_eq!(reqs.len(), 1);
        let req = reqs[0].to_ascii_lowercase();
        assert!(req.starts_with("get / http/1.1\r\n"), "{req}");
        assert!(req.contains("\r\nuser-agent: byovox/0.3.0\r\n"), "{req}");
        let mut names: Vec<&str> = req
            .lines()
            .skip(1)
            .take_while(|l| !l.is_empty())
            .filter_map(|l| l.split(':').next())
            .collect();
        names.sort_unstable();
        assert_eq!(
            names,
            ["accept", "accept-encoding", "host", "user-agent"],
            "{req}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn equal_older_unparsable_and_prerelease_releases_show_nothing() {
        let dir = temp_dir("nothing");
        let bodies = [
            release("v0.3.0"),
            release("0.3.0"),
            release("v0.2.9"),
            release("v0.4.0-rc1"),
            release("v0.4.0+build"),
            release("v0.4"),
            release("v0.4.0.1"),
            release("nightly"),
            release("v+1.0.0"),
            release("v0.04.0"),
            release("v00.4.0"),
            release("v0.3.01"),
            release("v0.4.0").replace(r#""prerelease":false"#, r#""prerelease":true"#),
            release("v0.4.0").replace("https://github.com/", "https://example.com/"),
        ];
        for (i, body) in bodies.iter().enumerate() {
            let srv = MockServer::start(200, body);
            let got = run(srv.url(), &dir.join(format!("{i}.json")), T0);
            assert_eq!(got, None, "{body}");
            assert_eq!(srv.requests().len(), 1, "{body}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn every_failure_is_one_quiet_line_and_no_notice() {
        let dir = temp_dir("failures");
        // Port 0 is never served, unlike a port bound and released, which another test can take.
        let port_0 = "http://127.0.0.1:0".to_string();
        let hanging = TcpListener::bind("127.0.0.1:0").unwrap();
        let hanging_url = format!("http://{}", hanging.local_addr().unwrap());
        let plain = TcpListener::bind("127.0.0.1:0").unwrap();
        let tls_url = format!("https://{}", plain.local_addr().unwrap());
        std::thread::spawn(move || {
            use std::io::Write;
            if let Ok((mut s, _)) = plain.accept() {
                let _ = s.write_all(
                    b"HTTP/1.1 400 Bad Request

",
                );
            }
        });
        let servers = [
            ("404", MockServer::start(404, r#"{"message":"Not Found"}"#)),
            ("500", MockServer::start(500, "{}")),
            ("not json", MockServer::start(200, "<html>")),
            ("no tag", MockServer::start(200, r#"{"html_url":"x"}"#)),
        ];
        let mut cases: Vec<(&str, String)> = servers
            .iter()
            .map(|(name, s)| (*name, s.url().to_string()))
            .collect();
        cases.push(("port 0", port_0));
        cases.push(("tls", tls_url));
        cases.push(("timeout", hanging_url));
        for (i, (name, url)) in cases.iter().enumerate() {
            let mut got = None;
            let log = logged(|| {
                got = Some(check(
                    url,
                    &dir.join(format!("{i}.json")),
                    T0,
                    "0.3.0",
                    Duration::from_millis(500),
                    Duration::from_millis(500),
                ))
            });
            assert_eq!(got, Some(None), "{name}");
            let lines: Vec<&str> = log.lines().collect();
            assert_eq!(lines.len(), 1, "{name}: {log}");
            assert!(lines[0].contains(" INFO "), "{name}: {log}");
            assert!(lines[0].contains("update check failed"), "{name}: {log}");
        }
        drop(hanging);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn one_request_per_day_across_restarts_whatever_the_result() {
        let dir = temp_dir("per_day");
        for (name, srv) in [
            ("failing", MockServer::start(404, "{}")),
            ("answering", MockServer::start(200, &release("v0.4.0"))),
        ] {
            let stamp = dir.join(format!("{name}.json"));
            run(srv.url(), &stamp, T0);
            assert!(stamp.exists(), "{name}: no stamp written");
            for later in [1, 3_600, DAY - 1] {
                run(srv.url(), &stamp, T0 + later);
            }
            assert_eq!(srv.requests().len(), 1, "{name}: retried inside a day");
            run(srv.url(), &stamp, T0 + DAY);
            assert_eq!(srv.requests().len(), 2, "{name}: not checked after a day");
            run(srv.url(), &stamp, T0 + DAY - 1);
            assert_eq!(
                srv.requests().len(),
                2,
                "{name}: a clock stepped back made a second request"
            );
            run(srv.url(), &stamp, T0);
            assert_eq!(
                srv.requests().len(),
                3,
                "{name}: a future stamp blocked the check"
            );
        }
        // A directory where the file should be makes the write fail.
        let srv = MockServer::start(200, &release("v0.4.0"));
        let blocked = dir.join("blocked.json");
        std::fs::create_dir_all(&blocked).unwrap();
        assert_eq!(run(srv.url(), &blocked, T0), None);
        assert!(srv.requests().is_empty(), "requested without a stamp");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_balloon_is_owed_once_per_release() {
        let dir = temp_dir("balloon");
        let stamp = dir.join("update.json");
        let v4 = MockServer::start(200, &release("v0.4.0"));
        let v5 = MockServer::start(200, &release("v0.5.0"));
        let balloon = |srv: &MockServer, now| run(srv.url(), &stamp, now).map(|r| r.balloon);
        assert_eq!(balloon(&v4, T0), Some(true));
        assert_eq!(balloon(&v4, T0 + DAY), Some(false));
        assert_eq!(balloon(&v4, T0 + 5 * DAY), Some(false));
        assert_eq!(balloon(&v5, T0 + 6 * DAY), Some(true));
        assert_eq!(balloon(&v5, T0 + 7 * DAY), Some(false));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn check_off_never_enters_the_check_path() {
        let dir = temp_dir("off");
        let stamp = dir.join("update.json");
        let srv = MockServer::start(200, &release("v9.0.0"));
        let handle = spawn(false, srv.url(), stamp.clone(), |_| {
            panic!("no release is reported with the check off")
        });
        assert!(handle.is_none());
        std::thread::sleep(Duration::from_millis(200));
        assert!(srv.requests().is_empty());
        assert!(!stamp.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    const ZIP: &str = "byovox-v0.4.0-x86_64-pc-windows-msvc.zip";

    fn zip_of(files: &[(&str, &[u8])]) -> Vec<u8> {
        use std::io::Write;
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let deflated = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, body) in files {
            w.start_file(*name, deflated).unwrap();
            w.write_all(body).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    fn release_zip() -> Vec<u8> {
        zip_of(&[
            ("byovox.exe", b"new cli"),
            ("byovox-daemon.exe", b"new daemon"),
            ("README.md", b"readme"),
        ])
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        ring::digest::digest(&ring::digest::SHA256, bytes)
            .as_ref()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    fn sums_for(zip: &[u8]) -> String {
        format!("{}  {ZIP}\n", sha256_hex(zip))
    }

    /// A release on loopback: the API reply on `api`, each asset on a server of its own.
    struct Loopback {
        api: MockServer,
        zip: MockServer,
        sums: MockServer,
    }

    fn loopback(base: &str, zip: &[u8], sums: &str, assets: &[&str]) -> Loopback {
        let zip_srv = MockServer::start_bytes(200, zip);
        let sums_srv = MockServer::start(200, sums);
        let listed: Vec<String> = assets
            .iter()
            .map(|name| {
                let size = if *name == "SHA256SUMS" {
                    sums.len()
                } else {
                    zip.len()
                };
                // A URL the update must not use: it builds its own from the tag.
                format!(
                    r#"{{"name":"{name}","browser_download_url":"https://elsewhere.invalid/{name}","size":{size}}}"#
                )
            })
            .collect();
        let body = base.replace(
            r#""assets":[]"#,
            &format!(r#""assets":[{}]"#, listed.join(",")),
        );
        Loopback {
            api: MockServer::start(200, &body),
            zip: zip_srv,
            sums: sums_srv,
        }
    }

    impl Loopback {
        fn at(&self) -> impl Fn(&str, &str) -> String + '_ {
            move |_, name| {
                let srv = if name == "SHA256SUMS" {
                    &self.sums
                } else {
                    &self.zip
                };
                format!("{}/{name}", srv.url())
            }
        }
    }

    #[test]
    fn assets_are_fetched_from_the_release_tag_on_github() {
        assert_eq!(
            release_download("v0.8.0", "SHA256SUMS"),
            "https://github.com/iteramen/byovox/releases/download/v0.8.0/SHA256SUMS"
        );
    }

    /// An install folder holding the old pair, and a scratch folder beside it.
    fn installed(test: &str) -> (PathBuf, PathBuf, PathBuf) {
        let root = temp_dir(test);
        let dir = root.join("byovox");
        let scratch = root.join("scratch");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&scratch).unwrap();
        std::fs::write(dir.join("byovox.exe"), "old cli").unwrap();
        std::fs::write(dir.join("byovox-daemon.exe"), "old daemon").unwrap();
        (root, dir, scratch)
    }

    fn plain() -> ureq::Agent {
        ureq::Agent::new_with_defaults()
    }

    fn files(dir: &Path) -> Vec<(String, String)> {
        let mut all: Vec<(String, String)> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| {
                let e = e.unwrap();
                (
                    e.file_name().into_string().unwrap(),
                    std::fs::read_to_string(e.path()).unwrap(),
                )
            })
            .collect();
        all.sort();
        all
    }

    fn untouched() -> Vec<(String, String)> {
        vec![
            ("byovox-daemon.exe".into(), "old daemon".into()),
            ("byovox.exe".into(), "old cli".into()),
        ]
    }

    #[test]
    fn an_update_stages_the_verified_pair_and_swap_puts_it_in_place() {
        let (root, dir, scratch) = installed("update");
        let zip = release_zip();
        let sums = format!("{}  other.zip\n{}", sha256_hex(b"other"), sums_for(&zip));
        let rel = loopback(&release("v0.4.0"), &zip, &sums, &[ZIP, "SHA256SUMS"]);
        let got = stage(&plain(), rel.api.url(), &rel.at(), "0.3.0", &dir, &scratch);
        assert_eq!(got.as_deref(), Ok("0.4.0"));
        assert_eq!(rel.zip.requests().len(), 1);
        assert_eq!(rel.sums.requests().len(), 1);
        assert!(files(&scratch).is_empty(), "the zip was left behind");
        swap(&dir).unwrap();
        let landed = files(&dir);
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(
            landed,
            [
                ("byovox-daemon.exe", "new daemon"),
                ("byovox-daemon.exe.old", "old daemon"),
                ("byovox.exe", "new cli"),
                ("byovox.exe.old", "old cli"),
            ]
            .map(|(n, b)| (n.to_string(), b.to_string()))
        );
    }

    #[test]
    fn a_checksum_mismatch_is_refused_and_nothing_is_staged() {
        let (root, dir, scratch) = installed("mismatch");
        let zip = release_zip();
        let rel = loopback(
            &release("v0.4.0"),
            &zip,
            &sums_for(b"some other zip"),
            &[ZIP, "SHA256SUMS"],
        );
        let got = stage(&plain(), rel.api.url(), &rel.at(), "0.3.0", &dir, &scratch);
        let (in_dir, in_scratch) = (files(&dir), files(&scratch));
        std::fs::remove_dir_all(&root).unwrap();
        let e = got.unwrap_err();
        assert!(e.contains("does not match"), "{e}");
        assert!(e.contains(&sha256_hex(&zip)), "{e}");
        assert_eq!(in_dir, untouched());
        assert!(in_scratch.is_empty(), "{in_scratch:?}");
    }

    #[test]
    fn a_missing_asset_checksum_line_or_exe_is_refused_and_nothing_is_staged() {
        let zip = release_zip();
        let half = zip_of(&[("byovox.exe", b"new cli")]);
        let both = vec![ZIP, "SHA256SUMS"];
        let cases = [
            ("no zip", &zip, sums_for(&zip), vec!["SHA256SUMS"], ZIP),
            ("no sums", &zip, sums_for(&zip), vec![ZIP], "SHA256SUMS"),
            (
                "no line",
                &zip,
                format!(
                    "{}  other.zip
",
                    sha256_hex(&zip)
                ),
                both.clone(),
                "no checksum",
            ),
            (
                "no daemon",
                &half,
                sums_for(&half),
                both,
                "byovox-daemon.exe",
            ),
        ];
        for (case, zip, sums, assets, named) in cases {
            let (root, dir, scratch) = installed(&format!("missing-{}", case.replace(' ', "-")));
            let rel = loopback(&release("v0.4.0"), zip, &sums, &assets);
            let got = stage(&plain(), rel.api.url(), &rel.at(), "0.3.0", &dir, &scratch);
            let in_dir = files(&dir);
            std::fs::remove_dir_all(&root).unwrap();
            let e = got.unwrap_err();
            assert!(e.contains(named), "{case}: {e}");
            assert_eq!(in_dir, untouched(), "{case}");
        }
    }

    #[test]
    fn a_release_not_newer_is_refused_before_any_download() {
        let zip = release_zip();
        for (tag, prerelease) in [("v0.3.0", false), ("v0.2.9", false), ("v0.4.0", true)] {
            let (root, dir, scratch) = installed(&format!("not-newer-{tag}-{prerelease}"));
            let mut base = release(tag);
            if prerelease {
                base = base.replace(r#""prerelease":false"#, r#""prerelease":true"#);
            }
            let rel = loopback(&base, &zip, &sums_for(&zip), &[ZIP, "SHA256SUMS"]);
            let got = stage(&plain(), rel.api.url(), &rel.at(), "0.3.0", &dir, &scratch);
            let in_dir = files(&dir);
            std::fs::remove_dir_all(&root).unwrap();
            let e = got.unwrap_err();
            assert!(e.contains("up to date"), "{tag}: {e}");
            assert!(rel.zip.requests().is_empty() && rel.sums.requests().is_empty());
            assert_eq!(in_dir, untouched(), "{tag}");
        }
    }

    #[test]
    fn a_cargo_install_or_a_folder_without_the_pair_is_refused_before_any_request() {
        for manifest in [".crates.toml", ".crates2.json"] {
            let (root, _, scratch) = installed(&format!("cargo{manifest}"));
            let bin = root.join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            std::fs::write(root.join(manifest), "").unwrap();
            for exe in EXES {
                std::fs::write(bin.join(exe), "cargo build").unwrap();
            }
            let api = MockServer::start(200, &release("v0.4.0"));
            let got = stage(
                &plain(),
                api.url(),
                &release_download,
                "0.3.0",
                &bin,
                &scratch,
            );
            std::fs::remove_dir_all(&root).unwrap();
            let e = got.unwrap_err();
            assert!(e.contains(CARGO_INSTALL), "{manifest}: {e}");
            assert!(api.requests().is_empty(), "{manifest}");
        }
        let (root, dir, scratch) = installed("no-daemon");
        std::fs::remove_file(dir.join("byovox-daemon.exe")).unwrap();
        let api = MockServer::start(200, &release("v0.4.0"));
        let got = stage(
            &plain(),
            api.url(),
            &release_download,
            "0.3.0",
            &dir,
            &scratch,
        );
        std::fs::remove_dir_all(&root).unwrap();
        assert!(got.unwrap_err().contains("byovox-daemon.exe is not in"));
        assert!(api.requests().is_empty());
    }

    #[test]
    fn the_real_agent_refuses_a_plain_http_release() {
        let (root, dir, scratch) = installed("https-only");
        let api = MockServer::start(200, &release("v0.4.0"));
        let got = stage(
            &deploy::agent(),
            api.url(),
            &release_download,
            "0.3.0",
            &dir,
            &scratch,
        );
        std::fs::remove_dir_all(&root).unwrap();
        assert!(got.is_err(), "{got:?}");
        assert!(api.requests().is_empty(), "a request reached the server");
    }

    #[test]
    fn a_failed_replace_puts_the_old_pair_back() {
        let (root, dir, _) = installed("rollback");
        // The daemon has nothing staged, so its replace fails after the CLI's has succeeded.
        std::fs::write(dir.join("byovox.exe.new"), "new cli").unwrap();
        let got = swap(&dir);
        let landed = files(&dir);
        std::fs::remove_dir_all(&root).unwrap();
        let e = got.unwrap_err();
        assert!(e.contains("replacing byovox-daemon.exe"), "{e}");
        assert_eq!(landed, untouched());
    }

    #[test]
    fn sha256sums_lines_are_read_as_sha256sum_writes_them() {
        let h = "A".repeat(64);
        let sums = format!(
            "{h}  a.zip\r\n{}  b.zip\n{} *c.zip\nshort  d.zip\n",
            "b".repeat(64),
            "c".repeat(64)
        );
        assert_eq!(sha256_for(&sums, "a.zip"), Some("a".repeat(64)));
        assert_eq!(sha256_for(&sums, "b.zip"), Some("b".repeat(64)));
        assert_eq!(sha256_for(&sums, "c.zip"), Some("c".repeat(64)));
        assert_eq!(sha256_for(&sums, "d.zip"), None);
        assert_eq!(sha256_for(&sums, "a"), None);
    }

    #[test]
    fn spawn_returns_while_the_request_is_still_hanging() {
        let dir = temp_dir("hanging");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (answer, answered) = mpsc::channel::<()>();
        let server = std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf);
            let _ = answered.recv_timeout(Duration::from_secs(20));
            let body = release("v999.0.0");
            let _ = write!(
                s,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        });
        let (tx, rx) = mpsc::channel::<Release>();
        let started = Instant::now();
        let handle = spawn(true, &url, dir.join("update.json"), move |r| {
            tx.send(r).unwrap()
        })
        .expect("the check is on");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "spawn waited for the request: {:?}",
            started.elapsed()
        );
        assert!(rx.try_recv().is_err(), "a release arrived before the reply");
        answer.send(()).unwrap();
        handle.join().unwrap();
        server.join().unwrap();
        assert_eq!(rx.recv().unwrap().version, "999.0.0");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
