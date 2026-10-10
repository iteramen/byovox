//! The local speech server `[stt.server]` names: started by the daemon with no window, and
//! ended with it.

use std::path::Path;
use std::process::{Child, Command, Stdio};

use crate::config::SttServer;

pub fn validate(s: &SttServer) -> Result<(), String> {
    if s.command.is_empty() {
        return Ok(());
    }
    let path = Path::new(&s.command);
    if !path.is_absolute() {
        return Err(format!(
            "stt.server.command `{}` is not an absolute path",
            s.command
        ));
    }
    if !path.is_file() {
        return Err(format!("stt.server.command `{}`: no such file", s.command));
    }
    // A .bat or .cmd would run through cmd.exe, and a shortcut cannot be started at all.
    #[cfg(windows)]
    if !path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("exe"))
    {
        return Err(format!("stt.server.command `{}` is not an .exe", s.command));
    }
    Ok(())
}

/// What ends the server. On Windows, a Job Object created with
/// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`: closing its last handle ends every process in it.
pub struct Job {
    #[cfg(windows)]
    _handle: std::os::windows::io::OwnedHandle,
}

pub fn spawn(command: &Path, args: &[String], stderr: Stdio) -> std::io::Result<(Child, Job)> {
    let mut cmd = Command::new(command);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let child = cmd.spawn()?;
    #[cfg(windows)]
    let job = match kill_on_close_job(&child) {
        Ok(job) => job,
        Err(e) => {
            let mut child = child;
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }
    };
    #[cfg(not(windows))]
    let job = Job {};
    Ok((child, job))
}

#[cfg(windows)]
fn kill_on_close_job(child: &Child) -> std::io::Result<Job> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    use windows::core::PCWSTR;

    // SAFETY: no security attributes (so the handle is not inheritable) and no name.
    let handle = unsafe { CreateJobObjectW(None, PCWSTR::null()) }?;
    // SAFETY: `handle` was just opened here and nothing else owns it.
    let job = Job {
        _handle: unsafe { OwnedHandle::from_raw_handle(handle.0) },
    };
    let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    // SAFETY: `info` is the struct `JobObjectExtendedLimitInformation` names, with its size passed.
    unsafe {
        SetInformationJobObject(
            handle,
            JobObjectExtendedLimitInformation,
            std::ptr::from_ref(&info).cast(),
            size_of_val(&info) as u32,
        )
    }?;
    // SAFETY: both handles are open for the call: the job and `child`'s process handle.
    unsafe { AssignProcessToJobObject(handle, HANDLE(child.as_raw_handle())) }?;
    Ok(job)
}

pub fn start(s: &SttServer) -> Option<u32> {
    if s.command.is_empty() {
        return None;
    }
    let (child, job) = match spawn(Path::new(&s.command), &s.args, Stdio::null()) {
        Ok(started) => started,
        Err(e) => {
            tracing::error!(command = %s.command, error = %e, "stt server failed to start");
            return None;
        }
    };
    // Never closed by hand. The daemon leaves through `std::process::exit`, which runs no
    // destructors, so it is the process exit closing this last handle that ends the server —
    // on Quit, on a crash and on a kill alike.
    let _ = std::mem::ManuallyDrop::new(job);
    let pid = child.id();
    tracing::info!(pid, command = %s.command, "stt server started");
    let _ = std::thread::Builder::new()
        .name("byovox-server".into())
        .spawn(move || watch(child))
        .inspect_err(|e| tracing::warn!(error = %e, "stt server started but not watched"));
    Some(pid)
}

fn watch(mut child: Child) {
    match child.wait() {
        Ok(status) => tracing::warn!(%status, "stt server exited"),
        Err(e) => tracing::warn!(error = %e, "stt server: waiting for it failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_relative_or_missing_command_is_refused_by_key_and_empty_is_accepted() {
        let with = |command: &str| SttServer {
            command: command.into(),
            args: Vec::new(),
        };
        assert_eq!(validate(&with("")), Ok(()), "empty starts nothing");

        let e = validate(&with("whisper-server.exe")).unwrap_err();
        assert!(e.contains("stt.server.command"), "{e}");
        assert!(e.contains("not an absolute path"), "{e}");

        let missing = std::env::temp_dir().join("byovox-no-such-server.exe");
        let e = validate(&with(missing.to_str().unwrap())).unwrap_err();
        assert!(e.contains("stt.server.command"), "{e}");
        assert!(e.contains("no such file"), "{e}");

        let exe = std::env::current_exe().unwrap();
        assert_eq!(
            validate(&with(exe.to_str().unwrap())),
            Ok(()),
            "an existing file"
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_batch_file_is_refused() {
        let bat = std::env::temp_dir().join(format!("byovox-server-{}.bat", std::process::id()));
        std::fs::write(&bat, "@echo off\r\n").unwrap();
        let e = validate(&SttServer {
            command: bat.to_str().unwrap().into(),
            args: Vec::new(),
        });
        std::fs::remove_file(&bat).unwrap();
        let e = e.unwrap_err();
        assert!(e.contains("stt.server.command"), "{e}");
        assert!(e.contains("not an .exe"), "{e}");
    }

    #[test]
    fn an_empty_command_starts_nothing_and_logs_nothing() {
        let mut pid = Some(0);
        let log = crate::testutil::logged(|| pid = start(&SttServer::default()));
        assert_eq!(pid, None);
        assert_eq!(log, "", "nothing logged");
    }

    #[test]
    fn a_spawn_failure_is_one_error_and_no_pid() {
        let missing = std::env::temp_dir().join("byovox-no-such-server.exe");
        let mut pid = Some(0);
        let log = crate::testutil::logged(|| {
            pid = start(&SttServer {
                command: missing.to_str().unwrap().into(),
                args: Vec::new(),
            })
        });
        assert_eq!(pid, None);
        assert_eq!(log.matches("ERROR").count(), 1, "{log}");
        assert!(log.contains("stt server failed to start"), "{log}");
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use std::time::Duration;

    use super::*;

    const PING: &str = r"C:\Windows\System32\PING.EXE";
    const AS_CONSOLE_PROBE: &str = "byovox-helper-console-probe";
    const AS_DAEMON: &str = "byovox-helper-daemon";
    const AS_CONSOLELESS_DAEMON: &str = "byovox-helper-consoleless-daemon";
    const NO_WINDOW: i32 = 7;

    fn ping_args() -> Vec<String> {
        ["-n", "30", "127.0.0.1"].map(String::from).to_vec()
    }

    fn exits_within(pid: u32, within: Duration) -> bool {
        use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
        use windows::Win32::System::Threading::{
            OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
        };
        // SAFETY: a plain open by pid; the handle is closed below.
        let Ok(h) = (unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) }) else {
            return true;
        };
        // SAFETY: `h` is open until the `CloseHandle` below.
        let waited = unsafe { WaitForSingleObject(h, within.as_millis() as u32) };
        // SAFETY: as above.
        let _ = unsafe { CloseHandle(h) };
        waited == WAIT_OBJECT_0
    }

    fn helper_args(marker: &str) -> Vec<String> {
        [
            "server::windows_tests::child_helper",
            marker,
            "--exact",
            "--ignored",
            "--nocapture",
        ]
        .map(String::from)
        .to_vec()
    }

    #[test]
    fn closing_the_job_ends_the_server() {
        let (mut child, job) = spawn(Path::new(PING), &ping_args(), Stdio::null()).unwrap();
        assert!(child.try_wait().unwrap().is_none(), "ping runs for 30 s");
        drop(job);
        assert!(
            exits_within(child.id(), Duration::from_secs(2)),
            "the server outlived its job"
        );
        let _ = child.wait();
    }

    fn daemon_with_server(end: impl FnOnce(&mut Child)) -> u32 {
        use std::io::BufRead;

        let mut daemon = Command::new(std::env::current_exe().unwrap())
            .args(helper_args(AS_DAEMON))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut lines = std::io::BufReader::new(daemon.stdout.take().unwrap()).lines();
        let mut seen = Vec::new();
        let pid: u32 = loop {
            let line = lines
                .next()
                .expect("the helper printed no server pid")
                .unwrap();
            if let Some(pid) = line.strip_prefix("server pid=") {
                break pid.parse().unwrap();
            }
            seen.push(line);
        };
        assert!(
            !exits_within(pid, Duration::from_millis(500)),
            "the server died while its daemon was still running"
        );
        end(&mut daemon);
        let _ = daemon.wait();
        // Before the drain: the server holds the daemon's inherited stdout, so reading it to
        // the end would wait for the server to exit on its own.
        assert!(
            exits_within(pid, Duration::from_secs(2)),
            "server alive 2s after the daemon ended"
        );
        seen.extend(lines.map_while(Result::ok));
        assert!(
            !seen.iter().any(|l| l.contains("127.0.0.1")),
            "the server's output reached the daemon's: {seen:?}"
        );
        pid
    }

    #[test]
    fn the_server_ends_when_the_daemon_exits() {
        let pid = daemon_with_server(|d| {
            use std::io::Write;
            writeln!(d.stdin.as_mut().unwrap(), "exit").unwrap();
        });
        assert!(
            exits_within(pid, Duration::from_secs(2)),
            "the server outlived a daemon that exited"
        );
    }

    #[test]
    fn the_server_ends_when_the_daemon_is_killed() {
        let pid = daemon_with_server(|d| d.kill().unwrap());
        assert!(
            exits_within(pid, Duration::from_secs(2)),
            "the server outlived a daemon that was killed"
        );
    }

    #[test]
    fn the_server_gets_no_console_window() {
        use std::os::windows::process::CommandExt;

        let status = Command::new(std::env::current_exe().unwrap())
            .args(helper_args(AS_CONSOLELESS_DAEMON))
            .creation_flags(0x0000_0008)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert_eq!(
            status.code(),
            Some(NO_WINDOW),
            "the server has a console window"
        );
    }

    #[test]
    fn a_server_that_exits_is_one_warn_with_its_exit_code() {
        let (child, _job) = spawn(
            Path::new(r"C:\Windows\System32\cmd.exe"),
            &["/c", "exit", "-1073741515"].map(String::from),
            Stdio::null(),
        )
        .unwrap();
        let log = crate::testutil::logged(|| watch(child));
        assert_eq!(log.matches("WARN").count(), 1, "{log}");
        assert!(log.contains("stt server exited"), "{log}");
        assert!(log.contains("0xc0000135"), "{log}");
    }

    #[test]
    #[ignore]
    fn child_helper() {
        let args: Vec<String> = std::env::args().collect();
        if args.iter().any(|a| a == AS_CONSOLE_PROBE) {
            // SAFETY: no arguments; reads this process's console window, if it has one.
            let window = unsafe { windows::Win32::System::Console::GetConsoleWindow() };
            std::process::exit(if window.is_invalid() { NO_WINDOW } else { 1 });
        }
        if args.iter().any(|a| a == AS_CONSOLELESS_DAEMON) {
            let exe = std::env::current_exe().unwrap();
            let (mut probe, _job) =
                spawn(&exe, &helper_args(AS_CONSOLE_PROBE), Stdio::null()).unwrap();
            std::process::exit(probe.wait().unwrap().code().unwrap_or(-1));
        }
        if args.iter().any(|a| a == AS_DAEMON) {
            let pid = start(&SttServer {
                command: PING.into(),
                args: ping_args(),
            })
            .expect("started");
            println!("server pid={pid}");
            let _ = std::io::stdin().read_line(&mut String::new());
            std::process::exit(0);
        }
    }
}
