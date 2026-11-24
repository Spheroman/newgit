//! Every instance ever spawned under a name shares its checkpoint history;
//! what keeps one instance's removal from reaching another's kept work
//! (second review of #465). `remove --purge` purges only its own
//! instance's checkpoints, never one an earlier instance's archived record
//! names; `cleanup` collects a `remove` checkpoint only when the same
//! instance took a later one; and an archived record never replaces
//! another.

mod evict_support;

use camino::Utf8PathBuf;
use evict_support::*;
use serde_json::Value;

fn json(run: &Run) -> Value {
    serde_json::from_str(&run.stdout)
        .unwrap_or_else(|error| panic!("not JSON ({error}):\n{}\n{}", run.stdout, run.stderr))
}

/// `x` spawned with work in it, removed: the checkpoint that keeps the
/// work, and the archived record that names it.
fn removed_with_work(repo: &Utf8PathBuf, temp: &Utf8PathBuf) -> (String, String) {
    newgit_ok(repo, &["spawn", "x"]);
    std::fs::write(temp.join("workspaces/x/work.txt"), "only copy\n").expect("write");
    let out = json(&newgit_ok(repo, &["remove", "x", "--json"]));
    (
        out["checkpoint"].as_str().expect("kept").to_owned(),
        out["archived_record"]
            .as_str()
            .expect("archived")
            .to_owned(),
    )
}

fn comes_back(repo: &Utf8PathBuf, temp: &Utf8PathBuf, id: &str) {
    let back = newgit(
        repo,
        &["spawn", "y", "--from-checkpoint", &format!("x/{id}")],
    );
    assert!(
        back.status.success() && temp.join("workspaces/y/work.txt").exists(),
        "the first `x`'s checkpoint {id} is gone: {}",
        back.stderr
    );
}

#[test]
fn purge_of_a_reused_name_keeps_the_earlier_instances_checkpoint() {
    let (_guard, temp) = tempdir("remove-reused-purge");
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();
    let (id, _) = removed_with_work(&repo, &temp);

    newgit_ok(&repo, &["spawn", "x"]);
    newgit_ok(&repo, &["checkpoint", "x", "-m", "the second x's"]);
    let second = json(&newgit_ok(&repo, &["remove", "x", "--purge", "--json"]));
    // Its own checkpoint went, and only that.
    assert_eq!(second["purged_checkpoints"], 1, "{second}");
    comes_back(&repo, &temp, &id);
}

#[test]
fn cleanup_keeps_a_remove_checkpoint_another_instance_did_not_retry() {
    let (_guard, temp) = tempdir("remove-reused-cleanup");
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();
    let (id, archived) = removed_with_work(&repo, &temp);

    newgit_ok(&repo, &["spawn", "x"]);
    let second = json(&newgit_ok(&repo, &["remove", "x", "--json"]));
    // As if the second archive had landed on the first's name: nothing
    // names the first checkpoint any more, and the second removal took a
    // later `remove` checkpoint. A later instance's is not a retry of it.
    let second_path = second["archived_record"].as_str().expect("archived");
    std::fs::rename(second_path, &archived).expect("overwrite");
    newgit_ok(&repo, &["cleanup"]);
    comes_back(&repo, &temp, &id);
}

#[test]
fn an_archived_record_never_replaces_another() {
    let (_guard, temp) = tempdir("remove-archive-unique");
    let store = setup(&temp);
    let repo = store.paths().project_root.clone();
    newgit_ok(&repo, &["spawn", "x"]);
    // Every name a removal in the next minute would take at one-second
    // resolution, already taken.
    let archived = store.paths().archived_branches.clone();
    std::fs::create_dir_all(&archived).expect("mkdir");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs();
    let mut taken = Vec::new();
    for second in 0..60 {
        let path = archived.join(format!("x-{}.toml", utc_stamp(now + second)));
        std::fs::write(&path, format!("earlier record {second}\n")).expect("write");
        taken.push(path);
    }
    let out = json(&newgit_ok(&repo, &["remove", "x", "--json"]));
    let path = out["archived_record"].as_str().expect("archived");
    assert!(!taken.iter().any(|taken| taken == path), "{path}");
    for (second, path) in taken.iter().enumerate() {
        assert_eq!(
            std::fs::read_to_string(path).expect("read"),
            format!("earlier record {second}\n"),
            "{path} was replaced"
        );
    }
    let record = std::fs::read_to_string(path).expect("read");
    assert!(record.contains("name = \"x\""), "{record}");
}

/// `%Y%m%dT%H%M%SZ` of `secs` since the epoch, as `archive_branch_record`
/// names a record (days to a civil date: Howard Hinnant's algorithm).
fn utc_stamp(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rest = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        rest / 3_600,
        rest / 60 % 60,
        rest % 60
    )
}
