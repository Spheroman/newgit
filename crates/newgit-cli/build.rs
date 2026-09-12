use std::process::Command;

/// Stamp the build with the commit it came from.
///
/// A pre-1.0 tool that rewrites workspaces and holds a project's branch
/// state has to be able to answer "which build did that?" — a bare `0.1.0`
/// cannot. Falls back cleanly when Git is absent or the source is a tarball,
/// because a missing commit must not fail the build.
fn main() {
    println!("cargo:rustc-env=NEWGIT_BUILD={}", build_stamp());
    // Only the recorded HEAD affects this; without these the stamp would go
    // stale until something else forced a rebuild.
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/refs/heads");
    println!("cargo:rerun-if-env-changed=NEWGIT_BUILD");
}

fn build_stamp() -> String {
    // A release workflow can pass the stamp in rather than shipping .git.
    if let Ok(stamp) = std::env::var("NEWGIT_BUILD")
        && !stamp.trim().is_empty()
    {
        return stamp;
    }

    let Some(commit) = git(&["rev-parse", "--short=12", "HEAD"]) else {
        return "unknown".to_owned();
    };
    // A dirty tree means the binary does not correspond to any commit, which
    // is exactly what a bug report needs to say out loud.
    let dirty = git(&["status", "--porcelain"]).is_some_and(|out| !out.is_empty());
    if dirty {
        format!("{commit}-dirty")
    } else {
        commit
    }
}

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}
