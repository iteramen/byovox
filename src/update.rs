//! The daily update check: one unauthenticated `GET` of the latest release, at most once per 24 h, never when `update.check = false`.
//! Yields a `Release` only for a newer `MAJOR.MINOR.PATCH`; every failure is one INFO line. Nothing is installed.

use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

pub const LATEST_URL: &str = "https://api.github.com/repos/iteramen/byovox/releases/latest";

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
}

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

/// A page that is not on github.com is refused: the tray hands it to `explorer`.
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
