# `newgit serve`

*The newgit remote as one binary: Git repositories on local disk, project
state in SQLite, and the same API as the Worker (`remote/`), so the CLI talks
to either. Tracked in [#344].*

[#344]: https://github.com/Spheroman/newgit/issues/344

---

## Why

The Worker ([remote.md](remote.md)) is slow mostly because of Artifacts and
the workarounds around it. Clones took 9–38 s against 1.8 s from GitHub.
Forks never finished for large repositories. Checks had to fetch whole
histories into a container, and the client polled while they ran. Each
request went client → Worker → Durable Object → container → Artifacts.
Cutting all of that out:

| | newgit.dev | `newgit serve` (local, 2026-11-09) |
|---|---|---|
| `remote add` | 9.4 s | 0.85 s |
| plain `git clone` | 2.6–10.8 s | 0.4 s |
| checkpoint + `git push` | 10.4 + 5.0 s | 1.7 s |
| `newgit merge` | 6.4 s | 1.2 s |
| `spawn --from-checkpoint` | 8.4 s | 1.1 s |

The local numbers have no internet round trips. Of the 1.1 s spawn, about
40 ms is on the server; the rest is the CLI's ~220 sequential Git processes.

## Running it

```
newgit serve --data /var/lib/newgit --listen 0.0.0.0:8484 \
  --url https://git.example.com --allow '*' --admin jack \
  --github-client-id <oauth app id>
newgit remote add --url https://git.example.com
```

TLS is for a proxy in front (Caddy, nginx), not the binary. For tests and
trying changes locally, `--dev-login-secret` (or
`NEWGIT_SERVE_DEV_LOGIN_SECRET`) lets a caller holding it log in as anyone
invited, through `POST /v1/dev/login` with `x-dev-login: <secret>`.
`crates/newgit/tests/serve.rs` runs the whole loop that way.

## The shape

```
<data>/
  server.sqlite          tokens, projects, members, branches (heads), instances,
                         forks, checkpoints, tags, links, grants, items
  token.key              the HMAC key tokens are signed with
  hooks/pre-receive      refuses refs outside a grant's prefix
  homes/<owner>/<project>/
    t/<tracker>.git      a tracker's history; its instances' forks as namespaces
    u/<tracker>/<login>.git   one user's per-user tracker
    i/<instance>/<tracker>.git  a private instance's fork
  endpoint/<owner>/<project>/   the composer's stores and cached views
```

- **One SQLite database, every project table keyed by `home` first.** The home
  (`<owner>/<project>` today) is the shard unit: moving a set of homes to
  another node is a `WHERE home …` and an `rsync` of `homes/<home>`. One
  connection behind one lock, as SQLite serializes writers anyway.
- **The API is the Worker's**, route for route, so the CLI does not change.
  `crates/newgit-serve/src/api.rs` mirrors `remote/src/index.ts`, and
  `policy.rs` ports `names.ts` `decide` and the definitions reader unchanged
  in meaning.
- **Real Git serves every byte.** `git upload-pack` / `receive-pack
  --stateless-rpc`, streamed; at most `--git-slots` at once.

## Decisions

Resolved loudly; each is reversible.

1. **A fork is a Git namespace in the tracker's repo**
   (`refs/namespaces/i/refs/namespaces/<instance>/`), made by copying the
   tracker's branch refs in. Nothing is copied, so there is nothing to wait
   for, nothing to garbage-collect around, and no deletion order to keep.
   The tracker's own readers are served with `transfer.hideRefs` over
   `refs/namespaces/`, and a push to the tracker cannot reach a namespace.
   **A private instance's forks are repos of their own:** objects sharing a
   store answer "do you have this?" for everyone in it, and an embargo's
   shape is the secret.
   **`hideRefs` only tidies the listing; it is not access control.** Under
   protocol v2 a reader can fetch any object in a repo by id ("wants can be
   anything", `gitprotocol-v2(5)`), a `have` line is an existence oracle,
   and a crafted push can point a ref at any object in the store, which a
   reachability check then allows. So a repo is safe only when everyone who
   can reach it may read all of it: **one audience per repo, always.** The
   namespaces here are safe because a fork's readers are exactly its
   tracker's. Reproduced in `docs/research/nixpkgs-bench.md`, *Review*.
2. **The Git password is the caller's own newgit token.** No repo passwords
   are minted, stored, or revoked. Each Git request is checked against the
   rules when it arrives, so a revocation or a removed member bites on the
   very next request. Credentials still come from `POST /credentials`, which
   says whether the rules would allow it and when to ask again.
3. **Grants are the exceptions to the standing rules**, time-limited and
   tied to one token: a new tracker's seed (any ref, until it joins a
   branch, and never once it is on one), and a merge's push to landing refs
   (`refs/newgit/landing/` only, enforced by the `pre-receive` hook).
4. **A merge's anchor decides approval, not what lands.** The Worker landed
   only the trackers at and under the anchor and rolled the parents up
   later, so merges at different anchors did not wait on each other across
   Durable Objects. Here a landing is one SQLite transaction over the whole
   index, compared with the merge's `base`, then each repo's branch moved
   before answering. The merger's own commits are the branch's history, ids
   and signatures kept, and nothing rolls up. Two merges begun from one
   index at different anchors now conflict, and the second merges again.
   Who must approve is decided by the anchor exactly as before.
5. **Checks run here, at once.** Ancestry, "only pins moved", the history
   walk, and what root's commit pins are local `git` reads. Nothing answers
   202.
6. **Definitions are cached until a tracker repo's own refs move.** They are
   read from trackers' `main`, which moves only through this server (a push
   to a repo's own refs, a branch made, a project made or deleted), so the
   cache is dropped on each of those, not after a time to live. A Git
   request went from 14 ms to 5 ms.
7. **Merges in flight are kept in memory.** A restart forgets them, and
   their finish is told to start the merge again, which is safe: nothing
   moved yet.

8. **Pull requests are here, not on the Worker** (#167, #170). newgit.dev
   is moving to `serve` ([landing.md](landing.md)), so a second copy in
   ProjectDO would be thrown away. The Worker answers 404 `no route`, and
   the CLI says the remote has no pull requests yet. They live in `items`,
   one number sequence per project for pull requests and, later, issues
   (#161). **A pull request names an instance and a base branch, never a
   commit:** its head is the instance's latest checkpoint, read when asked,
   so a push moves it, each checkpoint is a revision, and the push path did
   not change. One may be open per instance and branch. Opening one is
   refused while the workspace's HEAD is not the instance's head on the
   remote (unpushed commits) and for a private instance, which it would
   reveal. `pr checkout` is `spawn --from-checkpoint` on the head. A merge
   says which instance it lands (`instance` in the begin body), and the
   landing's own transaction marks that instance's pull request against the
   branch merged, so no moment shows the branch moved and the pull request
   open. Removing the instance closes its pull requests. The merger's word
   on which instance it is merging is taken as it is: merging needs the same
   approvals whatever it says, and the worst a false name does is mark that
   instance's pull request merged.

## Not yet

- The GitHub webhook (`POST /v1/webhooks/github`) and mirror pulls.
- The landing page and `/install.sh`.
- Moving newgit.dev across: copying every Artifacts repo and the Durable
  Objects' state, with a dry run; backups (Litestream for SQLite, bundles
  or a mirror for repos), and a rehearsed restore. Artifacts stays until
  that is proven.
- `bundle-uri` for clones from object storage, and replicas.
- The CLI's tests still run against `tests/support/fake_remote.rs`; moving
  them onto this server, with a test-only way to inject faults, replaces
  the fake.
- Removing what only Artifacts needed, on both sides, once the client
  changes with it: forks as an API concept, the objects upload route, and
  stepped pushes and fetches.
