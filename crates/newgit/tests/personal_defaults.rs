//! A person's own default per role (`~/.newgit/config.toml`) overrides the
//! project's `priority`, and `spawn --use` overrides both.

use std::process::{Command, Output};

use camino::{Utf8Path, Utf8PathBuf};

fn git(dir: &Utf8Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir.as_str())
        .args(args)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn newgit(home: &Utf8Path, dir: &Utf8Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_newgit"))
        .current_dir(dir.as_str())
        .args(args)
        .env("HOME", home.as_str())
        .output()
        .expect("newgit runs")
}

struct TempDir(Utf8PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tempdir() -> (TempDir, Utf8PathBuf) {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("newgit-defaults-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir tempdir");
    let path = Utf8PathBuf::from_path_buf(dir.canonicalize().expect("canonicalize"))
        .expect("utf8 tempdir");
    (TempDir(path.clone()), path)
}

fn dropped(store: &Utf8Path, instance: &str) -> String {
    std::fs::read_to_string(store.join(format!(".newgit/branches/{instance}.toml")))
        .expect("record")
        .lines()
        .skip_while(|line| !line.starts_with("dropped"))
        .take_while(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn a_personal_default_beats_priority_and_use_beats_both() {
    let (_guard, temp) = tempdir();
    let store = temp.join("store");
    std::fs::create_dir_all(&store).expect("mkdir");
    git(&store, &["init", "-q", "-b", "main"]);
    std::fs::write(store.join("README.md"), "hello\n").expect("write");
    git(&store, &["add", "."]);
    git(&store, &["commit", "-q", "-m", "initial"]);
    let home = temp.join("home");
    std::fs::create_dir_all(home.join(".newgit")).expect("mkdir");
    assert!(
        newgit(&home, &store, &["init", "--name", "shop"])
            .status
            .success()
    );
    let config = store.join(".newgit/config.toml");
    let mut text = std::fs::read_to_string(&config).expect("config");
    text.push_str(&format!("\n[workspace]\nroot = \"{}\"\n", temp.join("ws")));
    std::fs::write(&config, text).expect("write");
    for (name, priority) in [("dev-db", 1), ("staging-db", 0)] {
        std::fs::write(
            store.join(format!(".newgit/trackers/{name}.toml")),
            format!(
                "audience = \"user\"\nstorage = \"local\"\npaths = [\".env.db\"]\nrole = \"db\"\npriority = {priority}\n"
            ),
        )
        .expect("define");
    }

    // The project says dev-db; nothing personal yet.
    assert!(newgit(&home, &store, &["spawn", "a"]).status.success());
    assert!(dropped(&store, "a").contains("staging-db"));

    // Mine says staging-db.
    std::fs::write(
        home.join(".newgit/config.toml"),
        "[projects.shop.roles]\ndb = \"staging-db\"\n",
    )
    .expect("personal");
    assert!(newgit(&home, &store, &["spawn", "b"]).status.success());
    assert!(dropped(&store, "b").contains("dev-db"));

    // Asking beats both.
    assert!(
        newgit(&home, &store, &["spawn", "c", "--use", "dev-db"])
            .status
            .success()
    );
    assert!(dropped(&store, "c").contains("staging-db"));
}
