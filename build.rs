//! Stamps the built commit into `BYOVOX_GIT_SHA`; any failure yields `unknown`, never a build error.

fn main() {
    // HEAD alone goes stale after a commit: also watch the ref it names and `packed-refs`.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/packed-refs");
    if let Ok(head) = std::fs::read_to_string(".git/HEAD")
        && let Some(git_ref) = head.strip_prefix("ref: ")
    {
        println!("cargo:rerun-if-changed=.git/{}", git_ref.trim());
    }

    let sha = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_owned());

    println!("cargo:rustc-env=BYOVOX_GIT_SHA={sha}");
}
