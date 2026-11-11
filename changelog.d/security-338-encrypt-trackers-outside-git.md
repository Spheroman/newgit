- Trackers outside Git that sync (`storage = "remote"`) and are not
  `public` are encrypted to their audience before anything leaves the
  machine ([#338](https://github.com/Spheroman/newgit/issues/338)).

  Until now the remote stored their content in plaintext, so the audience
  on a `.env` tracker held against other users but not against whoever runs
  the remote. Each file is now encrypted whole with age, to the
  `ssh-ed25519` and `ssh-rsa` keys GitHub publishes for each member of the
  audience as the remote lists them, and committed as an ordinary blob. The
  recipients are computed, never read from the history; a record of them
  sits beside the files at `.newgit/recipients` for people to read. The
  workspace holds plaintext, owner-only, decrypted with `~/.ssh/id_ed25519`,
  `id_rsa`, or `$NEWGIT_IDENTITY`. Every recipient is printed by key
  fingerprint the first time and whenever the set changes. `members add`
  and `remove` re-encrypt `main` and fail when they could not (`members
  reencrypt` retries); removing someone does not take back what they could
  already read, and says so. Membership is read again before anything is
  published, and publishing a tracker's `main` no longer forces over a
  `main` merged elsewhere: it is refused until `newgit tracker pull`. There
  is no opt-out: newgit.dev's `site` is now `public`. An existing tracker is
  encrypted on its next capture, as a new history.

  What it does not do: the remote still decides membership, so it could add
  a login whose keys then become recipients (hence the fingerprints); GitHub
  accounts are the key authority; nothing is signed, so the remote can
  replace a file with content that decrypts cleanly
  ([#431](https://github.com/Spheroman/newgit/issues/431)); file names,
  sizes, commit messages, authors, and dates are visible; revs sent before
  encryption stay on the remote in plaintext, so rotate what they held; and
  a teammate on an older newgit sees ciphertext and would push plaintext, so
  everyone sharing a tracker has to upgrade.
