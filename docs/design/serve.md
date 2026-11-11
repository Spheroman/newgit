# `newgit serve`

*The newgit remote as one binary: Git stores on local disk, one per audience,
project state in SQLite, and the same API as the Worker (`remote/`), so the
CLI talks to either. Tracked in [#344]; the stores are [#409].*

[#344]: https://github.com/Spheroman/newgit/issues/344
[#409]: https://github.com/Spheroman/newgit/issues/409

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
                         forks, checkpoints, tags, links, grants, and which
                         store each tracker is in (tracker_stores)
  token.key              the HMAC key tokens are signed with
  hooks/pre-receive      refuses refs outside a grant's prefix
  homes/<owner>/<project>/stores/   one bare repository per audience, refs in reftable
    project-devs.git     trackers the members read (root and access by default)
    public.git           trackers anyone reads
    user/<login>.git     trackers for user:<login>, and <login>'s content of
                         every per-user tracker
    instance/<name>.git  a private instance's forks
    tracker/<name>.git   a new tracker's seed, alone until it joins a branch
  homes/<owner>/<project>/landing/<login>/<namespace>.git
                         a merge's objects until it lands, never served
  endpoint/<owner>/<project>/   the composer's stores and cached views
```

Every repo the API names is a Git namespace in one store, served with
`git --namespace`, so its readers see `refs/heads/main` and nothing else:

| Repo (API name) | Store | Namespace |
|---|---|---|
| `<o>.<p>.t.<tracker>` | its audience's | `t/<tracker>` |
| `<o>.<p>.i.<instance>.<tracker>` | the tracker's | `t/<tracker>/i/<instance>`, inside the tracker's |
| the same, private instance | `instance/<instance>` | `t/<tracker>/i/<instance>` |
| `<o>.<p>.t.<tracker>.<login>` | `user/<login>` | `u/<tracker>` |

So root's `main` is `refs/namespaces/t/refs/namespaces/root/refs/heads/main`
in `project-devs.git`. Stores need Git 2.45 or newer, for reftable; serve
refuses to start on an older one.

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

1. **One object store per audience, every tracker a Git namespace in it**
   ([landing.md](landing.md), *Trackers, storage, and mounts*). Never a repo
   per tracker (22k repos: 424k files, 143 s gc), and **never a store shared
   across audiences.** Under protocol v2 a reader can fetch any object in a
   repository by id ("wants can be anything", `gitprotocol-v2(5)`), a `have`
   line is an existence oracle, and a crafted push can point a ref at any
   object in the store, which a reachability check then allows. So a store
   is safe only when everyone who can reach it may read all of it, and
   nothing here filters in front of `upload-pack` pretending otherwise.
   Reproduced in `docs/research/nixpkgs-bench.md`, *Review*.
   - **A store is made for one audience and never takes a second.** Its
     audience is written in its config (`newgit.audience`) when it is made,
     and making it again for another is refused. Which store a tracker is in
     is a row in SQLite, not something worked out from its definition: the
     definitions are read from the stores.
   - **`transfer.hideRefs` only tidies a listing.** A tracker's readers do
     not see its instances' forks inside it, and a push cannot reach outside
     its namespace; neither is access control, and neither needs to be.
   - **A fork is a namespace inside its tracker's**, made by copying the
     tracker's branch refs in: nothing to wait for or garbage-collect around,
     and it moves with the tracker. Its readers are exactly its tracker's.
   - **A private instance, an embargo, has a store of its own**, as does
     each user's content: its shape is the secret.
   - **A new tracker's seed is alone** in `tracker/<name>` until it joins a
     branch. Whoever seeds it may not read every audience, and their push
     could name any object in a store it landed in.
   - **Every repo is served only from the store its tracker's current facts
     call for**: its audience's, its user's, or a private instance's own. A
     tracker, a fork of it, a user's content, and a merge's objects all pass
     one check (`ProjectContext::served`), which settles the tracker first
     and refuses anything else, including a store at the right path whose
     label names another audience.
   - **A tracker settles in its audience's store** when it joins a branch,
     when a landing changes its definition, and before any Git request
     reaches it, which is refused if it cannot move. The first tracker of an
     audience is its seed's store, labelled and then renamed; the rest are
     copied in, their row moved, and only then removed from their old store,
     each ref at exactly the object copied, so a push that moved one
     meanwhile fails the move (the row goes back) instead of being dropped.
     A move that stopped after the copy is finished by the next settle.
   - **A merge's objects wait in a landing of the merger's own**, never in a
     store, until the merge lands. Which store they belong in is the
     landing's to say: one that narrows a tracker's audience and adds to it
     in the same merge must not leave the addition in the wider store, where
     anyone reaching any repo in it could fetch it by id, with nothing to
     remove it. Uploaded packs and pushes with a merge's password both go
     into `landing/<login>/<namespace>.git`, which borrows the store's
     objects (alternates) so thin packs resolve, and is served to no one.
     When the merge finishes, its definitions are read from the commits it
     lands, each tracker moves to the store they call for, and only then do
     its new commits follow it there, all before the landing is written
     down: under the db lock nothing lands beside it, and a landing whose
     objects have nowhere safe to go (a store in the way made for another
     audience) does not land.
   - **What a tracker held before its audience narrowed stays where it
     was.** Its refs leave the wider store, but its objects remain there
     until gc, and under protocol v2 any reader of that store can still
     fetch them by id through any repo in it. That content was readable by
     that audience already, so this keeps nothing new from anyone; to take
     back something that was public, rewrite it, do not narrow it.
   - **`has_commit` asks the store, not the namespace.** Within one audience
     that is no leak, only a looser check than a repo per tracker gave.
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

## Known limits

For the landing log (#410), which makes each of these a step of a landing
rather than a side effect:

- **Moves run under the global db mutex**, inside `authorize` (any Git
  request or credential) and inside a merge's `finish`. A large tracker's
  first copy into a store blocks every other request while it runs.
- **Moves are a side effect of reads and of landings' hooks**
  (`settle_after_landing`, the settle before serving). They should be a
  landing-log step, retried until done, not attempted by whichever request
  arrives first. A move that fails part way through a landing that moves
  several trackers can leave a copy of one already moved in its new store
  though the landing did not happen; for a widening, that is history in the
  wider store before its definition says so.
- **`Repo::update_refs` is one namespace per transaction**, so a landing's
  branch moves across trackers are not one ref transaction.
- **A push already running when a move starts is not stopped.** A ref it
  moves is caught (the move fails and goes back); a ref it creates after
  the copy is kept in the old store and logged, not carried along.

## Not yet

- Garbage collection of the stores and of merge landings.

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
