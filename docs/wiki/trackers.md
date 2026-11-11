# Trackers

Every tracker is its own history. `root` owns `.` — whatever no more specific
tracker claims, `.newgit/` included — and starts as your project's Git
history; `access`, mounted at `.newgit/trackers/`, holds who may read what,
which only the project's owner changes by default (`maintainers = "owner"`).
Carve out a tracker in Git with `newgit tracker create electron --git --path
apps/electron`: one commit that mounts it there, with its history starting
at the carve. Carved inside another mounted tracker, it is that tracker's
child, defined in that tracker's own `.newgit/trackers/`: bound only where its
parent is, and brought along with it unless its own `priority` is negative. So
a large directory can stay out of workspaces while the parent's own repository
still holds it. Trackers are named like files: `t3code/repos` from the top of a
project, `repos` from inside `apps/t3code`, `..` to go up, and a leading `/` for
the full path, such as `/remote/<owner>/<project>/t3code/repos`. Without `--git` a tracker stays outside Git: captured, never
committed.

A tracker outside Git that syncs (`--storage remote`) and is not `public` is
encrypted to its audience before anything leaves your machine: each file with
[age](https://age-encryption.org), to the SSH keys GitHub publishes for each
member the remote lists (`https://github.com/<login>.keys`, `ssh-ed25519` or
`ssh-rsa`). Your workspace holds plaintext, decrypted with
`~/.ssh/id_ed25519` or `id_rsa`, or the key `$NEWGIT_IDENTITY` names (a key
without a passphrase, added to your GitHub account, keeps agents from
meeting a prompt). newgit prints who it encrypts to, by key fingerprint, the
first time and whenever that changes. `newgit members add` and `remove`
re-encrypt `main` for the new set of members, and nothing this machine had
not sent yet reaches the remote encrypted to someone removed since; someone
removed can still read what the remote held while they were a member, so
rotate those secrets. If a definition you pull would store a file this
machine has held encrypted without encryption (the tracker turned `public`
or into a tracker in Git, or a new `public` tracker claiming its path),
newgit refuses to capture or send it until you run `newgit tracker
allow-plaintext <path>`. The guard knows paths, not content: the same
content copied or moved to another path is not held back. If `newgit tracker merge` is refused because
someone merged since, `newgit tracker pull <tracker> --keep-mine` puts your
version on top of theirs (yours replace theirs file by file, and files you
deleted stay deleted; no content merge), and merging again publishes it. There is no opt-out:
a tracker a server must read (a page the remote serves) is `public`.

What that guarantees, and what it does not:

- The remote, and anyone who controls its deployment, cannot read what was
  encrypted before it acts: this holds against a remote that only stores
  and serves.
- A remote that acts can read what is captured afterwards. It decides
  membership, so it can add a login whose GitHub keys then become
  recipients, and the only signal is the fingerprints newgit prints
  (`CHANGED`, `+ <login>`). A machine encrypting a tracker for the first
  time has nothing to compare with.
- The remote also serves tracker definitions, and a definition decides
  whether a path is encrypted at all. A machine that has held a path's
  encrypted content (captured it encrypted, or decrypted it into a
  workspace, as a fresh clone's spawn does) refuses, as an error, to
  capture or send that path unencrypted, whatever tracker claims it now,
  until `newgit tracker allow-plaintext <path>` is run there. A machine
  that has never held the content has nothing to compare against and
  captures as the definition says. It is a confirmation step, not a
  boundary: an agent in the workspace can run `allow-plaintext` itself.
  It knows paths, not content: the same content copied or moved to
  another path is not held back.
- GitHub accounts are the key authority.
- There is no integrity: recipients' keys are public, so the remote can
  replace a file with content of its choosing that decrypts cleanly (signed
  tracker commits are [#431](https://github.com/Spheroman/newgit/issues/431)).
- File names, sizes, link targets, commit messages, authors, and dates are
  visible.
- Revs pushed before a tracker was encrypted stay on the remote in
  plaintext: rotate what they held.
- A teammate on an older newgit sees ciphertext, and a capture of theirs
  would push plaintext.

An instance works on what it binds:

```sh
newgit spawn fix-db --use staging-db --drop electron   # one per role; the rest as you say
newgit tracker drop electron --stash   # out of the working tree, history untouched
newgit tracker use electron            # back, with the stashed work
newgit status                          # ready / starting / not ready, and why
```

Trackers sharing a `role` are alternatives (`dev-db`, `staging-db`); a spawn
picks one by `--use`, then your own default in `~/.newgit/config.toml`, then
`priority`. A tracker can bring resources (they run only while it is bound) and
`require` others.

`pinned = true` holds a tracker where its parent has it: a change to it never
rolls up into the parent by itself, and moving the pin is a change to the
parent, for its maintainers to approve. A tracker can also live in another
project on the remote and be mounted here too (`newgit tracker link`), with
one history and its home's policy. `newgit reference tracker` has every key.
