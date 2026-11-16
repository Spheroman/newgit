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
  published, and every commit the remote does not have yet that is
  encrypted to anyone else is made again first, so nothing captured after
  a member's removal (or captured before it and not yet sent) reaches the
  remote encrypted to them. Publishing a tracker's `main` no longer forces
  over a `main` merged elsewhere: it is refused, and `newgit tracker pull
  <tracker> --keep-mine` puts your version on top of theirs (file by file,
  your deletions included, no content merge) so merging again publishes it. A workspace path this
  machine has held encrypted content at is never captured or sent
  unencrypted because a definition now puts it in a `public` tracker or a
  tracker in Git, under any name, until `newgit tracker allow-plaintext
  <path>` on this machine (a confirmation, not a boundary; it knows paths,
  compared ignoring case, not content, so the same content copied or moved
  to another path is not held back; a machine that never held the content
  has nothing to compare against). `newgit
  checkpoint` exits non-zero when its sync is refused for a security reason,
  keeping the checkpoint locally. A per-user tracker is encrypted to the login `newgit login` saved,
  never to whoever the remote says you are. There is no opt-out:
  newgit.dev's `site` is now `public`. An existing tracker is encrypted on
  its next capture, as a new history.

  What it does not do: the remote cannot read what was encrypted before it
  acts, but a remote that acts can read what is captured afterwards. It
  decides membership, so it can add a login whose keys then become
  recipients, and the printed fingerprints are the only signal; it serves
  the definitions, which a machine that never held a path's content takes
  as they are; GitHub
  accounts are the key authority; nothing is signed, so the remote can
  replace a file with content that decrypts cleanly
  ([#431](https://github.com/Spheroman/newgit/issues/431)); file names,
  sizes, commit messages, authors, and dates are visible; revs sent before
  encryption stay on the remote in plaintext, so rotate what they held; and
  a teammate on an older newgit sees ciphertext and would push plaintext, so
  everyone sharing a tracker has to upgrade.
