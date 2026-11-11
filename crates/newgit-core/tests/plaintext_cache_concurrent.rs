//! Writers of the same blob's plaintext at once (`status` decrypts for every
//! instance in parallel) each succeed, and the cache only ever holds whole
//! files.

use std::sync::{Arc, Barrier};

use camino::Utf8PathBuf;
use newgit_core::crypt::Reader;

#[test]
fn concurrent_remembers_of_one_blob_all_succeed() {
    let temp = tempfile::tempdir().expect("tempdir");
    let local = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).expect("utf8 tempdir");
    let reader = Reader::for_store(&local.join("trackers.git"));
    let plaintext: Vec<u8> = (0..64 * 1024).map(|i| (i % 251) as u8).collect();
    const WRITERS: usize = 8;

    for round in 0..50 {
        let oid = format!("{round:040x}");
        let barrier = Arc::new(Barrier::new(WRITERS));
        let results: Vec<_> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..WRITERS)
                .map(|_| {
                    let barrier = Arc::clone(&barrier);
                    let (reader, oid, plaintext) = (&reader, &oid, &plaintext);
                    scope.spawn(move || {
                        barrier.wait();
                        let remembered = reader.remember(oid, plaintext);
                        // Whatever is there is whole.
                        if let Some(cached) = reader.cached(oid) {
                            assert_eq!(cached.len(), plaintext.len(), "a partial cache file");
                        }
                        remembered
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("joined"))
                .collect()
        });
        for result in results {
            result.expect("every writer succeeds");
        }
        assert_eq!(reader.cached(&oid).as_deref(), Some(plaintext.as_slice()));
    }
}
