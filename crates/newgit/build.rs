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
    println!("cargo:rerun-if-changed=.cargo_vcs_info.json");
    println!("cargo:rerun-if-env-changed=NEWGIT_BUILD");
}

fn build_stamp() -> String {
    // A release workflow can pass the stamp in rather than shipping .git.
    if let Ok(stamp) = std::env::var("NEWGIT_BUILD")
        && !stamp.trim().is_empty()
    {
        return stamp;
    }

    // A crates.io install builds from a tarball with no .git, which is how
    // most people get this binary. `cargo publish` records the commit here,
    // and it outranks `git` below: when this file exists we are building a
    // packaged crate, so a surrounding repo (a vendor/ directory, say) would
    // otherwise stamp the wrong project's HEAD.
    if let Some(commit) = packaged_commit() {
        return commit;
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

/// The commit `cargo publish` stamped into `.cargo_vcs_info.json`, if this is
/// a packaged crate. Hand-parsed because a build dependency for one field of
/// one file cargo generates itself is not worth the compile time.
fn packaged_commit() -> Option<String> {
    let raw = std::fs::read_to_string(".cargo_vcs_info.json").ok()?;
    let after = raw.split_once("\"sha1\"")?.1.split_once(':')?.1;
    let sha = after.split('"').nth(1)?;
    // Anything else means the file is not what we think it is, and a wrong
    // commit is worse than an honest "unknown".
    if sha.len() != 40 || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    // `cargo package --allow-dirty` records this, and it means the same thing
    // the git path's suffix does: the tarball matches no commit.
    let dirty = raw
        .split_once("\"dirty\"")
        .and_then(|(_, rest)| rest.split_once(':'))
        .is_some_and(|(_, value)| value.trim_start().starts_with("true"));
    if dirty {
        Some(format!("{}-dirty", &sha[..12]))
    } else {
        Some(sha[..12].to_owned())
    }
}

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}
