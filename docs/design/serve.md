# `newgit serve`

*The newgit remote as one binary: Git stores on local disk, one per audience,
every landing committed to a landing log, project state in SQLite, and the
same API as the Worker (`remote/`), so the CLI talks to either. Tracked in
[#344]; the stores are [#409], the landing log [#410].*

[#344]: https://github.com/Spheroman/newgit/issues/344
[#409]: https://github.com/Spheroman/newgit/issues/409
[#410]: https://github.com/Spheroman/newgit/issues/410

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
  landing.git            the landing log: one commit per landing, the commit
                         point; its tree is every branch's list and every
                         tracker's store (refs in reftable, never served)
  server.sqlite          tokens, projects, members, branches (heads), instances,
                         forks, checkpoints, tags, links, grants, and which
                         store each tracker is in (tracker_stores); lists and
                         stores follow the landing log (landing_applied)
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
  another node is a `WHERE home …`, an `rsync` of `homes/<home>`, and their
  part of the landing log's tree (its paths start with the home; nothing
  splits it yet, see *Known limits*). One connection behind one lock, as
  SQLite serializes writers anyway.
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
     one check (`ProjectContext::served`), which reads where the tracker is
     from its row (which only a landing writes) and refuses anything else,
     including a store at the right path whose label names another
     audience. A read never moves storage.
   - **A tracker moves to its audience's store as part of a landing**: the
     one that puts it on a branch, or that changes its definition. The move
     is in the landing's entry (decision 8) and is put in place after the
     commit point: copied into the new store (the first tracker of an
     audience is its seed's store, labelled and then renamed), then its row
     flipped with the rest of the landing, then removed from the old store
     once every push that began there is done, each ref at exactly the
     object copied; a ref a push moved meanwhile is carried to the new
     store, never dropped.
   - **A merge's objects wait in a landing of the merger's own**, never in a
     store, until they may go into one. Which store they belong in is the
     landing's to say: one that narrows a tracker's audience and adds to it
     in the same merge must not leave the addition in the wider store, where
     anyone reaching any repo in it could fetch it by id, with nothing to
     remove it. Uploaded packs and pushes with a merge's password both go
     into `landing/<login>/<namespace>.git`, which borrows the store's
     objects (alternates) so thin packs resolve, and is served to no one.
     When the merge finishes, its definitions are read from the commits it
     lands, and before the landing is written down each tracker's new
     commits go only into a store whose readers may see them whether or
     not it lands: the store it is in when it stays or widens, the new one
     when it narrows. A move between audiences not ordered by inclusion
     (`user:a` to `user:b`, a private instance's to the members') writes
     them into neither: they go straight into the new store once the
     landing is written down. Nothing moves before that point, so a landing
     refused or never written down has made nothing more readable, and one
     written down is put in place whole (decision 8). Every store a landing
     moves a tracker to is checked to be made for its audience first, so
     one in the way refuses the landing.
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
   Durable Objects. Here a landing is one entry in the landing log over the
   whole index, checked against the merge's `base` under the lock, and in
   place before answering. The merger's own commits are the branch's
   history, ids and signatures kept, and nothing rolls up. Two merges begun
   from one index at different anchors now conflict, and the second merges
   again. Who must approve is decided by the anchor exactly as before.
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
   moved yet. One whose landing was written down before the restart is in
   place when the server is back, and its finish asked again says so.
8. **The landing log is the commit point** ([landing.md](landing.md),
   *Landing*, items 3 and 4; `crates/newgit-serve/src/landing.rs`).
   - **One hidden Git repository per server**, `<data>/landing.git`: this
     server is one shard. Each landing (a merge's finish, a branch made, a
     tracker made or moved between stores, a project deleted) is one
     commit, written with `git fast-import` with `core.fsync=committed`;
     its stamp is its place in that history (1 for the first). The
     commit's tree is the whole state, every branch's list as gitlinks
     under `<owner>/<project>/branches/<branch>/` and every tracker's store
     under `<owner>/<project>/stores/`, so "state at stamp T" is the tree
     of commit T (`GET …/branches/<branch>?at=<stamp>`), never a commit
     date. Its message holds the entry: lists, ref moves, store moves, and
     the commits a move takes from a merger's landing. Gitlinks name
     commits without holding them, so the log holds no content and is
     served to no one.
   - **Everything else follows it.** After the commit, the landing is put
     in place: trackers it moves are copied into their new stores; then,
     under the db lock, every ref it moves (one `update-ref --stdin`
     transaction per store, full refnames across namespaces) and every row
     (one SQLite transaction), so a reader holding the lock (every API
     request, every Git request's authorization) sees all of it or none;
     then what moved is removed where it was. `landing_applied` in SQLite
     says how far that got. At startup, before anything is served, every
     entry after it is put in place again (each step is safe twice), so a
     crash between the log and the refs is repaired. Readers resolve heads
     and stores from those rows, never from a move in progress.
   - **Group commit.** The first landing to arrive leads: it takes
     everything waiting, checks each in order (the branch still at its
     `base`, each tracker still in the store it was prepared against, each
     asked ref still at its old commit) against what is there and what the
     batch lands before it, writes them with one `fast-import` (each its own
     commit), and puts them in place together, while the next ones queue
     behind it. 2,000 landings from 100 threads, each making a branch (one
     entry, one ref move, one row): about 390 a second in batches of 50,
     release build, one M5 Pro Mac (`landing_log.rs`, `landings_per_second`;
     nixpkgs-bench.md measured 382 for its pooled layout).
   - **Roots' locks.** A landing takes the lock of every project it touches
     at once (so in a fixed order, and a tracker linked into several
     projects stays atomic with all of them). Only one batch is checked
     and written at a time; a landing whose roots are held waits.
   - **A landing that moves storage holds its roots, not the log.** Copying
     a tracker into another store can be large, so it is put in place by
     its own request's thread after the batch is written, holding only its
     projects' locks, never the db lock or the log: other projects land
     meanwhile, and reads go on.
   - **A landing written down and not in place blocks its roots, loudly.**
     If putting one in place fails (a disk full, a store unwritable), its
     merger is told it landed and is not yet in place, nothing shows it,
     and every later landing on those projects first tries it again, and
     is refused with 503 while it still fails. Startup tries it too.
   - **A push into a store is counted while it runs.** A move waits for
     every push that began where the tracker was (before its row moved)
     before removing it there, so a ref a push in flight makes or moves is
     carried to the new store, not left behind in the old one.

## Known limits

- **Fast-forward only.** A merge lands only onto the `base` it began from,
  so it always fast-forwards; one whose branch moved meanwhile is told to
  merge again. Writing a merge commit with the merger's commits as its
  second parent when the branch moved needs the server to merge
  (`merge-tree`), which is [#411]: it writes that commit before the
  landing, and the log takes it like any other.
- **Raw Git reads are atomic per repo, not across repos.** Each store's
  refs move in one transaction and the rows that say where everything is
  move under the same lock, but a client fetching two trackers' repos one
  after another, while a landing is put in place between the two, can see
  one before it and one after. The API's lists and the project's Git
  endpoint (composed from those lists) never do.
- **A move into a wider store copies under its projects' lock.** A
  widening cannot copy before the landing is written down (its history
  would be readable by the wider audience before it was allowed to be), so
  its first copy into the wider store holds that project's landings (not
  reads, not other projects) while it runs.
- **One log per server.** Moving a set of homes to another node means
  splitting the log, which nothing does yet. A landing that touches a
  linked tracker on another server is two landings (landing.md), each in
  its own server's log.
- **A push that runs past ten minutes during a move** is no longer waited
  for: what it leaves in the old store stays there, and is logged.
- **Pushes that move no tracker's branch are not landings**: a fork's, a
  seed's, a per-user repo's, a tracker outside Git's. They move their own
  refs, in their own store, as before.

[#411]: https://github.com/Spheroman/newgit/issues/411
[#434]: https://github.com/Spheroman/newgit/issues/434

## Not yet

- Garbage collection of the stores, of merge landings, and of the landing
  log ([#434]).

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
