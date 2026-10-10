#![cfg_attr(windows, windows_subsystem = "windows")]
//! The byovox daemon: tray, keyboard hook, pipeline and IPC server, in a process with no console (a console-subsystem process would die with its terminal).

use std::path::PathBuf;

use clap::Parser;

#[derive(Parser)]
#[command(
    name = "byovox-daemon",
    version = byovox::VERSION,
    about = "The byovox daemon — started by `byovox`, not by hand"
)]
struct Cli {
    /// Config file (default: the platform config dir)
    #[arg(long)]
    config: Option<PathBuf>,
}

/// Inherited NUL handles give console children (the tray's `Run check`) a console wired to NUL; NULL handles give a real one.
#[cfg(windows)]
fn release_inherited_stdio() {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Console::{
        STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle,
    };

    for id in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: SetStdHandle only records a handle in the process's own parameters.
        let _ = unsafe { SetStdHandle(id, HANDLE::default()) };
    }
}

fn main() {
    let cli = Cli::parse();
    #[cfg(windows)]
    release_inherited_stdio();
    let result = byovox::daemon::run(byovox::daemon::Options {
        config_path: cli.config,
        log_to_stderr: false,
    });
    if let Err(e) = result {
        // stderr is NULL by now; the exit code is what `try_wait` sees.
        eprintln!("byovox-daemon: {e:#}");
        std::process::exit(2);
    }
}
