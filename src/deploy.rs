//! The whisper.cpp server and models `byovox setup` downloads, pinned by URL, size and SHA-256.

use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::config::SttServer;

pub struct Pin {
    pub url: &'static str,
    pub bytes: u64,
    pub sha256: &'static str,
}

impl Pin {
    pub fn file_name(&self) -> &'static str {
        self.url.rsplit('/').next().unwrap_or(self.url)
    }
}

pub const BUILD_TAG: &str = "b5130";

pub const PINS: [Pin; 7] = [
    Pin {
        url: "https://github.com/ggml-org/whisper.cpp/releases/download/b5130/whisper-bin-x64.zip",
        bytes: 8_573_270,
        sha256: "f9ec6c52a2e949b62ab51fa21d0d497958f9e41c3010c157c4e42932d5316f3c",
    },
    Pin {
        url: "https://github.com/ggml-org/whisper.cpp/releases/download/b5130/whisper-blas-bin-x64.zip",
        bytes: 21_360_234,
        sha256: "55c06d09e8b9b6cfb2b0b47ddedc71803054f0e48be1f41848b3141c06c703a9",
    },
    Pin {
        url: "https://github.com/ggml-org/whisper.cpp/releases/download/b5130/whisper-cublas-12.4.0-bin-x64.zip",
        bytes: 674_539_285,
        sha256: "af520ddd034d985b55dfeea3e465ed93653ba2aee1a55e865033edc548c272a7",
    },
    Pin {
        url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/5359861c739e955e79d9a303bcbc70fb988958b1/ggml-large-v3-turbo.bin",
        bytes: 1_624_555_275,
        sha256: "1fc70f774d38eb169993ac391eea357ef47c88757ef72ee5943879b7e8e2bc69",
    },
    Pin {
        url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/5359861c739e955e79d9a303bcbc70fb988958b1/ggml-large-v3-turbo-q5_0.bin",
        bytes: 574_041_195,
        sha256: "394221709cd5ad1f40c46e6031ca61bce88931e6e088c188294c6d5a55ffa7e2",
    },
    Pin {
        url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/5359861c739e955e79d9a303bcbc70fb988958b1/ggml-small-q8_0.bin",
        bytes: 264_464_607,
        sha256: "49c8fb02b65e6049d5fa6c04f81f53b867b5ec9540406812c643f177317f779f",
    },
    Pin {
        url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/5359861c739e955e79d9a303bcbc70fb988958b1/ggml-base-q8_0.bin",
        bytes: 81_768_585,
        sha256: "c577b9a86e7e048a0b7eada054f4dd79a56bbfa911fbdacf900ac5b567cbb7d9",
    },
];

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Build {
    Cpu,
    Blas,
    Cuda,
}

impl Build {
    pub fn pin(self) -> &'static Pin {
        &PINS[self as usize]
    }

    pub fn dir(self) -> &'static str {
        match self {
            Build::Cpu => "cpu",
            Build::Blas => "blas",
            Build::Cuda => "cuda-12.4",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Build::Cpu => "CPU",
            Build::Blas => "CPU with OpenBLAS",
            Build::Cuda => "NVIDIA CUDA 12.4",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Model {
    Turbo,
    TurboQ5,
    SmallQ8,
    BaseQ8,
}

impl Model {
    pub fn pin(self) -> &'static Pin {
        &PINS[3 + self as usize]
    }
}

/// Where the downloaded server listens, and the only address it is told to bind.
pub const HOST: &str = "127.0.0.1";
pub const PORT: u16 = 8770;

pub const SERVER_EXE: &str = "whisper-server.exe";

/// The banner line whisper.cpp prints on stderr when it found no GPU to run on.
pub const NO_GPU: &str = "whisper_backend_init_gpu: no GPU found";

/// `STATUS_DLL_NOT_FOUND`: the exit code of a process Windows could not load a DLL for.
pub const DLL_NOT_FOUND: i32 = 0xC000_0135_u32 as i32;

pub const MISSING_DLL: &str = "whisper-server could not start: a DLL it needs is missing. \
Install the Microsoft Visual C++ 2015-2022 Redistributable (x64) from \
https://aka.ms/vs/17/release/vc_redist.x64.exe, then run byovox setup again.";

const HEALTH_WAIT: Duration = Duration::from_secs(120);

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a download may wait for its response headers, or for any further bytes of its
/// body, before it fails. A wait, not a total: a slow download that keeps arriving never hits it.
const STALL_TIMEOUT: Duration = Duration::from_secs(30);

pub fn root() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "byovox")
        .map(|d| d.data_local_dir().join(format!("whisper-{BUILD_TAG}")))
}

pub fn size(bytes: u64) -> String {
    let mb = bytes as f64 / 1e6;
    if mb >= 1000.0 {
        format!("{:.2} GB", mb / 1000.0)
    } else if mb >= 100.0 {
        format!("{mb:.0} MB")
    } else {
        format!("{mb:.1} MB")
    }
}

/// Whether `command` is a server an earlier setup downloaded: a file that exists under
/// `root()`, reached without a `..`.
pub fn is_downloaded(command: &str) -> bool {
    root().is_some_and(|r| is_under(Path::new(command), &r))
}

fn is_under(path: &Path, root: &Path) -> bool {
    path.starts_with(root)
        && !path.components().any(|c| c == Component::ParentDir)
        && path.is_file()
}

pub fn port_in_use(addr: &str) -> bool {
    use std::net::ToSocketAddrs;
    addr.to_socket_addrs().is_ok_and(|mut all| {
        all.any(|a| std::net::TcpStream::connect_timeout(&a, Duration::from_secs(1)).is_ok())
    })
}

pub fn nvidia_driver() -> bool {
    std::env::var_os("SystemRoot")
        .is_some_and(|r| Path::new(&r).join("System32").join("nvcuda.dll").is_file())
}

/// HTTPS only, redirects included. No whole-body timeout: a model is 1.6 GB.
pub fn agent() -> ureq::Agent {
    agent_with(true, STALL_TIMEOUT)
}

/// `timeout_recv_response`, `timeout_global` and `timeout_per_call` stay unset: each would cap the whole body.
fn agent_with(https_only: bool, stall: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .https_only(https_only)
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_send_request(Some(stall))
        .timeout_recv_body(Some(stall))
        .build()
        .into()
}

fn matches_pin(path: &Path, pin: &Pin) -> bool {
    if !std::fs::metadata(path).is_ok_and(|m| m.len() == pin.bytes) {
        return false;
    }
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut sha = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buf = vec![0u8; 1 << 16];
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => sha.update(&buf[..n]),
            Err(_) => return false,
        }
    }
    hex(sha.finish()) == pin.sha256
}

fn hex(digest: ring::digest::Digest) -> String {
    digest.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// Downloads to `<name>.part` and renames only when size and SHA-256 match the pin; a file already
/// under its final name is re-verified and kept or replaced. There is no resume.
pub fn fetch(agent: &ureq::Agent, pin: &Pin, dir: &Path) -> Result<PathBuf, String> {
    let name = pin.file_name();
    let path = dir.join(name);
    if path.is_file() {
        if matches_pin(&path, pin) {
            return Ok(path);
        }
        println!("  {name} does not match its pin: downloading it again.");
        std::fs::remove_file(&path).map_err(|e| format!("{name}: deleting it: {e}"))?;
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    let part = dir.join(format!("{name}.part"));
    let got = download(agent, pin, &part);
    let failed = match got {
        Ok((bytes, sha256)) if bytes == pin.bytes && sha256 == pin.sha256 => {
            return std::fs::rename(&part, &path)
                .map(|()| path)
                .map_err(|e| format!("{name}: renaming the verified download: {e}"));
        }
        Ok((bytes, _)) if bytes > pin.bytes => format!(
            "{name} does not match its pin: expected {} bytes, got more than {}. The download was \
             deleted.",
            pin.bytes, pin.bytes
        ),
        Ok((bytes, sha256)) => format!(
            "{name} does not match its pin: expected {} bytes with SHA-256 {}, got {bytes} bytes \
             with SHA-256 {sha256}. The download was deleted.",
            pin.bytes, pin.sha256
        ),
        Err(e) => format!("{name}: {e}"),
    };
    let _ = std::fs::remove_file(&part);
    Err(failed)
}

/// Reading stops one byte past `pin.bytes`: that byte is already a mismatch, and a body that
/// never ends must not fill the disk.
fn download(agent: &ureq::Agent, pin: &Pin, part: &Path) -> Result<(u64, String), String> {
    let mut response = agent.get(pin.url).call().map_err(|e| e.to_string())?;
    let mut body = response.body_mut().as_reader().take(pin.bytes + 1);
    let mut file = std::fs::File::create(part).map_err(|e| e.to_string())?;
    let mut sha = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buf = vec![0u8; 1 << 16];
    let mut done: u64 = 0;
    let total_mb = pin.bytes / 1_000_000;
    let mut shown = None;
    loop {
        let n = body.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        sha.update(&buf[..n]);
        file.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        done += n as u64;
        let mb = done / 1_000_000;
        if shown != Some(mb) {
            shown = Some(mb);
            print!("\r  {}: {mb} / {total_mb} MB", pin.file_name());
            let _ = std::io::stdout().flush();
        }
    }
    println!();
    Ok((done, hex(sha.finish())))
}

/// Only an entry directly in the zip's `Release` folder, named `whisper-server.exe` or ending in `.dll`,
/// with a single plain path component (no `..`, drive or folder), is unpacked.
fn unpacked_name(entry: &str) -> Option<&str> {
    let mut parts = entry.split(['/', '\\']);
    let (Some("Release"), Some(file), None) = (parts.next(), parts.next(), parts.next()) else {
        return None;
    };
    let mut components = Path::new(file).components();
    let plain = matches!(
        (components.next(), components.next()),
        (Some(Component::Normal(_)), None)
    );
    let wanted = file == SERVER_EXE
        || Path::new(file)
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("dll"));
    (plain && wanted).then_some(file)
}

/// The server is renamed into place last, so a `whisper-server.exe` in `dir` means a whole build.
pub fn extract(zip: &Path, dir: &Path) -> Result<(), String> {
    let failed = |e: &dyn std::fmt::Display| format!("unpacking {}: {e}", zip.display());
    let file = std::fs::File::open(zip).map_err(|e| failed(&e))?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| failed(&e))?;
    std::fs::create_dir_all(dir).map_err(|e| failed(&e))?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| failed(&e))?;
        let Some(name) = unpacked_name(entry.name()).map(str::to_owned) else {
            continue;
        };
        let to = if name == SERVER_EXE {
            dir.join(format!("{SERVER_EXE}.part"))
        } else {
            dir.join(&name)
        };
        let mut out = std::fs::File::create(to).map_err(|e| failed(&e))?;
        std::io::copy(&mut entry, &mut out).map_err(|e| failed(&e))?;
    }
    drop(archive);
    std::fs::rename(dir.join(format!("{SERVER_EXE}.part")), dir.join(SERVER_EXE))
        .map_err(|e| failed(&e))?;
    std::fs::remove_file(zip).map_err(|e| failed(&e))
}

fn unpack(agent: &ureq::Agent, pin: &Pin, root: &Path, dir: &Path) -> Result<(), String> {
    if dir.join(SERVER_EXE).is_file() {
        return Ok(());
    }
    let zip = fetch(agent, pin, root)?;
    extract(&zip, dir)
}

pub fn smoke_failure(code: i32) -> Option<String> {
    match code {
        0 => None,
        DLL_NOT_FOUND => Some(MISSING_DLL.to_string()),
        other => Some(format!("whisper-server -h exited with {:#x}", other as u32)),
    }
}

fn smoke(exe: &Path) -> Result<(), String> {
    let mut cmd = Command::new(exe);
    cmd.arg("-h").stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW (0x08000000).
        cmd.creation_flags(0x0800_0000);
    }
    let out = cmd
        .output()
        .map_err(|e| format!("whisper-server -h: {e}"))?;
    match out.status.code().and_then(smoke_failure) {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

pub struct Installed {
    pub server: String,
    pub model: String,
}

/// Server first, so a machine that cannot run it is told before the model downloads.
pub fn install(build: Build, model: Model) -> Result<Installed, String> {
    let root = root().ok_or("this platform names no local data directory")?;
    let agent = agent();
    let dir = root.join(build.dir());
    unpack(&agent, build.pin(), &root, &dir)?;
    let server = dir.join(SERVER_EXE);
    smoke(&server)?;
    let model = fetch(&agent, model.pin(), &root)?;
    let utf8 = |p: PathBuf| {
        p.into_os_string()
            .into_string()
            .map_err(|p| format!("{} is not a UTF-8 path", Path::new(&p).display()))
    };
    Ok(Installed {
        server: utf8(server)?,
        model: utf8(model)?,
    })
}

/// `Some(n)` appends `-t n`; a parameter, not `available_parallelism`, so tests stay deterministic.
pub fn server_args(model: &str, threads: Option<usize>) -> Vec<String> {
    let port = PORT.to_string();
    let mut args: Vec<String> = [
        "-m",
        model,
        "--host",
        HOST,
        "--port",
        &port,
        "-l",
        "auto",
        "--inference-path",
        "/v1/audio/transcriptions",
    ]
    .map(String::from)
    .to_vec();
    if let Some(n) = threads {
        args.push("-t".into());
        args.push(n.to_string());
    }
    args
}

pub fn base_url() -> String {
    format!("http://{HOST}:{PORT}/v1")
}

/// The `host:port` ureq connects to for `base_url` (default port 443 or 80) and `/health` at its root,
/// with no path and no userinfo. `None` when ureq could not send to `base_url` either.
pub fn server_endpoint(base_url: &str) -> Option<(String, String)> {
    use ureq::http::{Uri, uri::Scheme};
    let uri: Uri = base_url.parse().ok()?;
    let (scheme, default) = match uri.scheme()? {
        s if *s == Scheme::HTTPS => ("https", 443),
        s if *s == Scheme::HTTP => ("http", 80),
        _ => return None,
    };
    let addr = format!("{}:{}", uri.host()?, uri.port_u16().unwrap_or(default));
    let health = format!("{scheme}://{addr}/health");
    Some((addr, health))
}

/// Starts the server `s` names with its stderr kept, waits up to 120 s for `/health` to answer anything
/// but 503, runs `f`, then ends it. Returns `f`'s result and the stderr, or `None` if it never started.
pub fn with_server<T>(
    s: &SttServer,
    health: &str,
    f: impl FnOnce(&mut std::process::Child) -> T,
) -> Option<(T, String)> {
    let (mut child, job) =
        match crate::server::spawn(Path::new(&s.command), &s.args, Stdio::piped()) {
            Ok(started) => started,
            Err(e) => {
                println!("  the server did not start: {e}");
                return None;
            }
        };
    // Drained on a thread of its own: a server blocked on a full pipe never answers `/health`.
    let reader = child.stderr.take().map(|mut err| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = err.read_to_end(&mut bytes);
            String::from_utf8_lossy(&bytes).into_owned()
        })
    });
    if let Err(e) = wait_healthy(&mut child, health) {
        println!("  {e}");
    }
    let out = f(&mut child);
    // Closing the job ends the server, which closes the pipe the reader is draining. Elsewhere
    // the job holds nothing, so the server is killed directly.
    #[cfg(windows)]
    drop(job);
    #[cfg(not(windows))]
    {
        let _ = job;
        let _ = child.kill();
    }
    let _ = child.wait();
    let stderr = reader.and_then(|r| r.join().ok()).unwrap_or_default();
    Some((out, stderr))
}

/// whisper.cpp answers 503 while its model loads; a server with no `/health` answers 404,
/// which ureq returns as `Error::StatusCode`, and that ends the wait too.
fn wait_healthy(child: &mut std::process::Child, url: &str) -> Result<(), String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(2)))
        .build()
        .into();
    let deadline = Instant::now() + HEALTH_WAIT;
    println!("  waiting for {url} while the server loads its model...");
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!("the server exited ({status}) before it answered"));
        }
        let answered = match agent.get(url).call() {
            Ok(_) => true,
            Err(ureq::Error::StatusCode(status)) => status != 503,
            Err(_) => false,
        };
        if answered {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "the server did not answer {url} within {} s",
                HEALTH_WAIT.as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::MockServer;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("byovox-deploy-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        hex(ring::digest::digest(&ring::digest::SHA256, bytes))
    }

    fn pin_for(server: &MockServer, file: &str, bytes: u64, sha256: String) -> Pin {
        Pin {
            url: format!("{}/{file}", server.url()).leak(),
            bytes,
            sha256: sha256.leak(),
        }
    }

    fn plain_agent() -> ureq::Agent {
        agent_with(false, STALL_TIMEOUT)
    }

    #[test]
    fn every_pin_is_https_on_one_of_the_two_upstreams_with_a_sha256_and_its_size() {
        const GITHUB: &str = "https://github.com/ggml-org/whisper.cpp/releases/download/b5130/";
        const HF: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/5359861c739e955e79d9a303bcbc70fb988958b1/";
        for pin in &PINS {
            let on = [GITHUB, HF]
                .iter()
                .filter(|p| pin.url.starts_with(**p))
                .count();
            assert_eq!(on, 1, "{} is on exactly one upstream prefix", pin.url);
            assert_eq!(pin.sha256.len(), 64, "{}", pin.url);
            assert!(
                pin.sha256
                    .chars()
                    .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
                "{} is lowercase hex",
                pin.sha256
            );
        }
        let bytes: Vec<u64> = PINS.iter().map(|p| p.bytes).collect();
        assert_eq!(
            bytes,
            [
                8573270, 21360234, 674539285, 1624555275, 574041195, 264464607, 81768585
            ],
            "the pinned sizes"
        );
        assert_eq!(
            Build::Cuda.pin().file_name(),
            "whisper-cublas-12.4.0-bin-x64.zip"
        );
        assert_eq!(
            Model::TurboQ5.pin().file_name(),
            "ggml-large-v3-turbo-q5_0.bin"
        );
        assert_eq!(Model::SmallQ8.pin().file_name(), "ggml-small-q8_0.bin");
        assert_eq!(Model::BaseQ8.pin().file_name(), "ggml-base-q8_0.bin");
    }

    #[test]
    fn server_args_appends_threads_only_when_given() {
        let with = server_args("m.bin", Some(12));
        assert!(
            with.ends_with(&["-t".to_string(), "12".to_string()]),
            "{with:?}"
        );
        let idx = with
            .iter()
            .position(|a| a == "/v1/audio/transcriptions")
            .unwrap();
        assert_eq!(idx, with.len() - 3, "-t 12 comes right after it: {with:?}");

        let without = server_args("m.bin", None);
        assert_eq!(
            without,
            [
                "-m",
                "m.bin",
                "--host",
                HOST,
                "--port",
                &PORT.to_string(),
                "-l",
                "auto",
                "--inference-path",
                "/v1/audio/transcriptions",
            ],
            "no -t"
        );
    }

    #[test]
    fn the_real_agent_refuses_plain_http() {
        let server = MockServer::start(200, "model bytes");
        let dir = scratch("https-only");
        let pin = pin_for(&server, "m.bin", 11, sha256_hex(b"model bytes"));
        let got = fetch(&agent(), &pin, &dir);
        assert!(got.is_err(), "fetched over plain HTTP: {got:?}");
        let e = got.unwrap_err();
        assert!(e.contains("m.bin"), "{e}");
        assert!(server.requests().is_empty(), "a request reached the server");
        assert!(!dir.join("m.bin").exists() && !dir.join("m.bin.part").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_right_bytes_are_kept_under_their_name_and_no_part_file_is_left() {
        let server = MockServer::start(200, "model bytes");
        let dir = scratch("right");
        let pin = pin_for(&server, "m.bin", 11, sha256_hex(b"model bytes"));
        let path = fetch(&plain_agent(), &pin, &dir).unwrap();
        assert_eq!(path, dir.join("m.bin"));
        assert_eq!(std::fs::read(&path).unwrap(), b"model bytes");
        assert!(!dir.join("m.bin.part").exists(), "a part file was left");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn wrong_bytes_leave_no_file_and_name_both_hashes() {
        let server = MockServer::start(200, "tampered!!!");
        let dir = scratch("wrong-hash");
        let expected = sha256_hex(b"model bytes");
        let pin = pin_for(&server, "m.bin", 11, expected.clone());
        let got = fetch(&plain_agent(), &pin, &dir);
        assert!(got.is_err(), "wrong bytes were kept: {got:?}");
        let e = got.unwrap_err();
        assert!(e.contains(&expected), "the pinned hash: {e}");
        assert!(
            e.contains(&sha256_hex(b"tampered!!!")),
            "the actual hash: {e}"
        );
        assert!(!dir.join("m.bin").exists(), "no final file");
        assert!(!dir.join("m.bin.part").exists(), "no part file");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_wrong_length_leaves_no_file_and_names_both_sizes() {
        let server = MockServer::start(200, "model bytes");
        let dir = scratch("wrong-length");
        let pin = pin_for(&server, "m.bin", 12, sha256_hex(b"model bytes"));
        let got = fetch(&plain_agent(), &pin, &dir);
        assert!(got.is_err(), "a wrong length was kept: {got:?}");
        let e = got.unwrap_err();
        assert!(e.contains("expected 12 bytes"), "{e}");
        assert!(e.contains("got 11 bytes"), "{e}");
        assert!(!dir.join("m.bin").exists(), "no final file");
        assert!(!dir.join("m.bin.part").exists(), "no part file");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_existing_file_is_kept_only_when_it_still_matches_its_pin() {
        let server = MockServer::start(200, "model bytes");
        let dir = scratch("existing");
        let pin = pin_for(&server, "m.bin", 11, sha256_hex(b"model bytes"));
        std::fs::write(dir.join("m.bin"), "model bytez").unwrap();
        fetch(&plain_agent(), &pin, &dir).unwrap();
        assert_eq!(
            std::fs::read(dir.join("m.bin")).unwrap(),
            b"model bytes",
            "a same-size file with the wrong hash was kept"
        );
        assert_eq!(server.requests().len(), 1, "downloaded again");
        fetch(&plain_agent(), &pin, &dir).unwrap();
        assert_eq!(
            server.requests().len(),
            1,
            "a matching file was fetched again"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_overlong_body_is_cut_off_one_byte_past_the_pin() {
        let body = "x".repeat(100_000);
        let server = MockServer::start(200, &body);
        let dir = scratch("overlong");
        let pin = pin_for(&server, "m.bin", 10, sha256_hex(body.as_bytes()));
        let (read, _) = download(&plain_agent(), &pin, &dir.join("m.bin.part")).unwrap();
        assert_eq!(read, 11, "read past the pin");
        let e = fetch(&plain_agent(), &pin, &dir).unwrap_err();
        assert!(e.contains("expected 10 bytes, got more than 10"), "{e}");
        assert!(!dir.join("m.bin").exists() && !dir.join("m.bin.part").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_slow_download_that_keeps_arriving_is_not_cut_off() {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/m.bin", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let _ = s.read(&mut [0u8; 4096]);
            let _ = s.write_all(
                b"HTTP/1.1 200 OK
Content-Length: 20

",
            );
            for _ in 0..20 {
                std::thread::sleep(Duration::from_millis(100));
                let _ = s.write_all(b"x");
            }
        });
        let dir = scratch("slow");
        let pin = Pin {
            url: url.leak(),
            bytes: 20,
            sha256: sha256_hex(&[b'x'; 20]).leak(),
        };
        let got = fetch(&agent_with(false, Duration::from_millis(300)), &pin, &dir);
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(got.is_ok(), "a slow download was cut off: {got:?}");
    }

    #[test]
    fn a_stalled_server_fails_the_download_within_the_stall_timeout() {
        use std::net::TcpListener;
        for (case, reply) in [
            ("no headers", ""),
            (
                "mid-body",
                "HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nabc",
            ),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}/m.bin", listener.local_addr().unwrap());
            std::thread::spawn(move || {
                let (mut s, _) = listener.accept().unwrap();
                let _ = s.read(&mut [0u8; 4096]);
                let _ = s.write_all(reply.as_bytes());
                // Silent, then gone: a download without a stall timeout would wait for this.
                std::thread::sleep(Duration::from_secs(5));
            });
            let dir = scratch(&format!("stall-{}", case.replace(' ', "-")));
            let pin = Pin {
                url: url.leak(),
                bytes: 100,
                sha256: "0000000000000000000000000000000000000000000000000000000000000000",
            };
            let started = Instant::now();
            let got = fetch(&agent_with(false, Duration::from_millis(300)), &pin, &dir);
            let took = started.elapsed();
            std::fs::remove_dir_all(&dir).unwrap();
            assert!(got.is_err(), "{case}: {got:?}");
            assert!(
                took < Duration::from_secs(3),
                "{case}: the stalled download took {took:?}"
            );
        }
    }

    #[test]
    fn an_unpacked_build_is_not_fetched_again() {
        let server = MockServer::start(200, "zip bytes");
        let dir = scratch("unpacked");
        let build_dir = dir.join("cpu");
        std::fs::create_dir_all(&build_dir).unwrap();
        std::fs::write(build_dir.join(SERVER_EXE), "exe").unwrap();
        let pin = pin_for(&server, "b.zip", 9, sha256_hex(b"zip bytes"));
        let got = unpack(&plain_agent(), &pin, &dir, &build_dir);
        let fetched = server.requests().len();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(fetched, 0, "the build was fetched again");
        assert_eq!(got, Ok(()));
    }

    #[test]
    fn only_an_existing_file_under_the_download_folder_counts_as_downloaded() {
        let dir = scratch("under");
        let root = dir.join("whisper-b5130");
        std::fs::create_dir_all(root.join("cpu")).unwrap();
        std::fs::write(root.join("cpu").join(SERVER_EXE), "exe").unwrap();
        std::fs::write(dir.join("elsewhere.exe"), "exe").unwrap();
        let inside = is_under(&root.join("cpu").join(SERVER_EXE), &root);
        let missing = is_under(&root.join("blas").join(SERVER_EXE), &root);
        let outside = is_under(&dir.join("elsewhere.exe"), &root);
        let climbing = is_under(&root.join("..").join("elsewhere.exe"), &root);
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(inside, "the downloaded server");
        assert!(!missing, "a file that is not there");
        assert!(!outside, "a file outside the folder");
        assert!(!climbing, "a path that climbs out with ..");
    }

    #[test]
    fn only_the_server_and_its_dlls_are_unpacked_and_nothing_escapes() {
        let dir = scratch("extract");
        let target = dir.join("cpu");
        let zip_path = dir.join("build.zip");
        {
            let mut w = zip::ZipWriter::new(std::fs::File::create(&zip_path).unwrap());
            let deflated = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            for (name, body) in [
                ("Release/whisper-server.exe", "exe"),
                ("Release/a.dll", "dll"),
                ("Release/readme.txt", "txt"),
                ("Release/../evil.dll", "evil"),
            ] {
                w.start_file(name, deflated).unwrap();
                w.write_all(body.as_bytes()).unwrap();
            }
            w.finish().unwrap();
        }
        let archive = zip::ZipArchive::new(std::fs::File::open(&zip_path).unwrap()).unwrap();
        assert!(
            archive.file_names().any(|n| n == "Release/../evil.dll"),
            "the climbing entry is in the zip as written"
        );
        drop(archive);
        extract(&zip_path, &target).unwrap();

        let mut landed: Vec<String> = std::fs::read_dir(&target)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        landed.sort();
        assert_eq!(landed, ["a.dll", "whisper-server.exe"]);
        assert_eq!(
            std::fs::read_to_string(target.join("whisper-server.exe")).unwrap(),
            "exe"
        );
        assert!(!dir.join("evil.dll").exists(), "written beside the target");
        assert!(!zip_path.exists(), "the zip is deleted after extraction");
        let mut left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, ["cpu"], "nothing else in the parent");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn the_server_is_up_while_f_runs_and_ended_after_it() {
        let health = MockServer::start(200, "{}");
        let s = SttServer {
            command: r"C:\Windows\System32\PING.EXE".into(),
            args: ["-n", "30", "127.0.0.1"].map(String::from).to_vec(),
        };
        let started = Instant::now();
        let (alive, _) = with_server(&s, &format!("{}/health", health.url()), |child| {
            child.try_wait().unwrap().is_none()
        })
        .expect("PING.EXE starts");
        assert!(alive, "the server was not running while f ran");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the server was not ended after f: with_server took {:?}",
            started.elapsed()
        );
        assert!(!health.requests().is_empty(), "health was polled");
    }

    #[cfg(windows)]
    fn ping(n: &str) -> std::process::Child {
        Command::new(r"C:\Windows\System32\PING.EXE")
            .args(["-n", n, "127.0.0.1"])
            .stdout(Stdio::null())
            .spawn()
            .expect("PING.EXE starts")
    }

    #[cfg(windows)]
    #[test]
    fn a_server_on_another_port_is_waited_for_on_that_port() {
        let health = MockServer::start(200, "{}");
        let s = SttServer {
            command: r"C:\Windows\System32\PING.EXE".into(),
            args: ["-n", "30", "127.0.0.1"].map(String::from).to_vec(),
        };
        let started = Instant::now();
        let (_, url) = server_endpoint(&format!("{}/v1", health.url())).unwrap();
        let (polled, _) =
            with_server(&s, &url, |_| !health.requests().is_empty()).expect("PING.EXE starts");
        assert!(polled, "{url} was not polled before f ran");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "with_server took {:?}",
            started.elapsed()
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_404_health_is_ready_at_once() {
        let health = MockServer::start(404, "{}");
        let mut child = ping("30");
        let started = Instant::now();
        let waited = wait_healthy(&mut child, &format!("{}/health", health.url()));
        let took = started.elapsed();
        let running = child.try_wait().unwrap().is_none();
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(waited, Ok(()), "a 404 was not taken as ready");
        assert!(took < Duration::from_secs(5), "the wait took {took:?}");
        assert!(
            running,
            "the server had exited: the wait did not end on the 404"
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_503_health_keeps_the_wait_going() {
        let health = MockServer::start(503, "{}");
        let mut child = ping("3");
        let waited = wait_healthy(&mut child, &format!("{}/health", health.url()));
        let _ = child.kill();
        child.wait().unwrap();
        assert!(
            waited
                .as_ref()
                .is_err_and(|e| e.starts_with("the server exited")),
            "a 503 ended the wait: {waited:?}"
        );
        assert!(
            health.requests().len() >= 2,
            "polled {} times",
            health.requests().len()
        );
    }

    #[test]
    fn the_endpoint_is_the_base_urls_host_and_port_as_ureq_reads_them() {
        for (base_url, addr, health) in [
            (
                "http://127.0.0.1:9100/v1",
                "127.0.0.1:9100",
                "http://127.0.0.1:9100/health",
            ),
            (
                "http://u:p@[::1]:9100/v1",
                "[::1]:9100",
                "http://[::1]:9100/health",
            ),
            ("http://[::1]/v1", "[::1]:80", "http://[::1]:80/health"),
            (
                "http://127.0.0.1:/v1",
                "127.0.0.1:80",
                "http://127.0.0.1:80/health",
            ),
            (
                "http://localhost/v1",
                "localhost:80",
                "http://localhost:80/health",
            ),
            (
                "HTTPS://box.lan/v1",
                "box.lan:443",
                "https://box.lan:443/health",
            ),
            (
                &base_url(),
                "127.0.0.1:8770",
                "http://127.0.0.1:8770/health",
            ),
        ] {
            assert_eq!(
                server_endpoint(base_url),
                Some((addr.to_string(), health.to_string())),
                "{base_url}"
            );
        }
    }

    #[test]
    fn an_unusable_base_url_has_no_endpoint_and_no_password_is_kept() {
        for base_url in [
            "127.0.0.1:9100/v1",
            " http://127.0.0.1:9100/v1",
            "ftp://box.lan/v1",
        ] {
            assert_eq!(server_endpoint(base_url), None, "{base_url:?}");
        }
        for base_url in [
            "http://user:abc/def@127.0.0.1:9100/v1",
            "http://user:abc#d@box/v1",
        ] {
            let got = format!("{:?}", server_endpoint(base_url));
            assert!(
                !got.contains("abc"),
                "{base_url}: the password was kept in {got}"
            );
        }
    }

    #[test]
    fn port_in_use_tries_every_address_a_name_resolves_to() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(
            port_in_use(&format!("localhost:{port}")),
            "a listener on 127.0.0.1 only was missed at localhost:{port}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn the_download_folder_is_whisper_b5130_under_localappdata_byovox_data() {
        let local = std::env::var_os("LOCALAPPDATA").unwrap();
        assert_eq!(
            root().unwrap(),
            Path::new(&local)
                .join("byovox")
                .join("data")
                .join("whisper-b5130"),
            "the download folder"
        );
    }

    #[test]
    fn a_missing_dll_exit_names_the_redistributable_word_for_word() {
        assert_eq!(
            smoke_failure(-1073741515).as_deref(),
            Some(
                "whisper-server could not start: a DLL it needs is missing. Install the \
                 Microsoft Visual C++ 2015-2022 Redistributable (x64) from \
                 https://aka.ms/vs/17/release/vc_redist.x64.exe, then run byovox setup again."
            ),
            "the redistributable message, word for word"
        );
        assert_eq!(smoke_failure(0), None);
        assert_eq!(
            smoke_failure(1).as_deref(),
            Some("whisper-server -h exited with 0x1")
        );
    }
}
