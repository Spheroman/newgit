//! Checks that start at once on a new container share one store, and making
//! it raced: both found it missing, both ran `git init` in it, and the second
//! failed on the first's files. Seen live with two merges begun together.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel".

use std::collections::BTreeMap;

use camino::Utf8PathBuf;
use newgit_core::endpoint::{AncestryPlan, check_ancestry};

#[test]
fn checks_starting_at_once_on_a_new_store_all_succeed() {
    for round in 0..5 {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir =
            Utf8PathBuf::from_path_buf(temp.path().join(format!("work-{round}"))).expect("utf8");
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let plan = AncestryPlan {
                    dir: dir.clone(),
                    repos: BTreeMap::new(),
                    checks: Vec::new(),
                };
                std::thread::spawn(move || check_ancestry(&plan))
            })
            .collect();
        for thread in threads {
            let answered = thread.join().expect("the check ran");
            assert!(answered.is_ok(), "{answered:?}");
        }
        assert!(dir.join("trackers.git/HEAD").exists());
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .expect("read")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".init-"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "scratch repos left behind: {leftovers:?}"
        );
    }
}
