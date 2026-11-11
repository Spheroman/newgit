// `newgit serve` keeps refs in reftable, which Git has from 2.45 on: on an
// older Git it refuses to start, naming the Git it found and the one it needs,
// rather than failing later on the first store it makes.

use std::process::Command;

#[test]
fn serve_refuses_a_git_older_than_reftable() {
    let dir = std::env::temp_dir().join(format!("newgit-serve-old-git-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("bin")).expect("mkdir");
    let fake = dir.join("bin/git");
    std::fs::write(
        &fake,
        "#!/bin/sh\necho 'git version 2.39.5 (Apple Git-154)'\n",
    )
    .expect("write");
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    let path = format!(
        "{}:{}",
        dir.join("bin").display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let output = Command::new(env!("CARGO_BIN_EXE_newgit"))
        .args(["serve", "--listen", "127.0.0.1:0", "--allow", "*", "--data"])
        .arg(dir.join("data"))
        .env("PATH", path)
        .output()
        .expect("runs");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "it started: {stderr}");
    assert!(
        stderr.contains("Git 2.45 or newer") && stderr.contains("git version 2.39.5"),
        "{stderr}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
