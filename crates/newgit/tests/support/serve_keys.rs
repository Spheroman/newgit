// SSH keys for `newgit serve` tests whose trackers outside Git are encrypted
// to their audience (#338). Included after `support/serve.rs`, whose `run`
// reads members' public keys from `<HOME>/published-keys`.

/// Give each of `logins` (`(login, home)`) an ed25519 key in its home's
/// `~/.ssh`, and publish every public half into every one of those homes'
/// `published-keys`, as `https://github.com/<login>.keys` would.
fn publish_keys(logins: &[(&str, &Utf8Path)]) {
    let mut published = Vec::new();
    for (login, home) in logins {
        let private = home.join(".ssh/id_ed25519");
        std::fs::create_dir_all(home.join(".ssh")).expect("mkdir");
        if !private.exists() {
            let status = Command::new("ssh-keygen")
                .args([
                    "-q",
                    "-t",
                    "ed25519",
                    "-N",
                    "",
                    "-C",
                    login,
                    "-f",
                    private.as_str(),
                ])
                .status()
                .expect("ssh-keygen runs");
            assert!(status.success(), "ssh-keygen failed");
        }
        let public = std::fs::read_to_string(format!("{private}.pub")).expect("public key");
        published.push((login.to_string(), public));
    }
    for (_, home) in logins {
        let dir = home.join("published-keys");
        std::fs::create_dir_all(&dir).expect("mkdir");
        for (login, public) in &published {
            std::fs::write(dir.join(format!("{login}.keys")), public).expect("publish");
        }
    }
}
