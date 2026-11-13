# `newgit serve`

*The newgit remote as one binary: Git stores on local disk, one per audience,
every landing committed to a landing log, project state in SQLite, and the
same API as the Worker (`remote/`), so the CLI talks to either. Tracked in
[#344]; the stores are [#409], the landing log [#410], the server's own
merge [#411], stored rooted histories [#412], mirrors [#414].*

[#344]: https://github.com/Spheroman/newgit/issues/344
[#409]: https://github.com/Spheroman/newgit/issues/409
[#410]: https://github.com/Spheroman/newgit/issues/410
[#411]: https://github.com/Spheroman/newgit/issues/411
[#412]: https://github.com/Spheroman/newgit/issues/412
[#440]: https://github.com/Spheroman/newgit/issues/440
[#414]: https://github.com/Spheroman/newgit/issues/414

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

`--github <file>` (or `NEWGIT_SERVE_GITHUB`) says what GitHub is to this
server (decision 11); without it there are no webhooks and no exports.

TLS is for a proxy in front (Caddy, nginx), not the binary. For tests and
trying changes locally, `--dev-login-secret` (or
`NEWGIT_SERVE_DEV_LOGIN_SECRET`) lets a caller holding it log in as anyone
invited, through `POST /v1/dev/login` with `x-dev-login: <secret>`.
`crates/newgit/tests/serve.rs` runs the whole loop that way.

## The shape

```
<data>/
  landing.git            the landing log: one commit per landing, the commit
                         point; its tree is every branch's list, every
                         tracker's store, and each project's incarnation
                         (refs in reftable, never served)
  server.sqlite          tokens, projects, members, branches (heads), instances,
                         forks, checkpoints, tags, links, grants, and which
                         store each tracker is in (tracker_stores); lists and
                         stores follow the landing log (landings_placed,
                         landing_applied, landings_voided)
  serve.lock             locked while a server runs: one per data directory
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
  homes/<owner>/<project>/scratch/<random>.git
                         the server's merge's composed objects while it
                         runs, never served, removed after it (and at start);
                         a mirror pull's and an export's too
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
| `/<o>/<p>[/<path>].git`, its stored view | the tracker's | `r/<tracker>` (decision 10) |

So root's `main` is `refs/namespaces/t/refs/namespaces/root/refs/heads/main`
in `project-devs.git`. Stores need Git 2.46 or newer, for reftable (2.45)
and symbolic refs moved in a ref transaction (`symref-update`, 2.46);
serve refuses to start on an older one.

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
   - **A fork of a tracker outside Git** (`tracker create --from`, #415) is
     made on its first write, starting at its upstream's branches, by the
     same landing that makes any tracker's repo (`api::Authorized::made`).
     Before anything is made or copied it is refused when the fork is wider
     than its upstream or the caller may not read the upstream
     (`policy::may_start_fork`), and what it starts at is read, all under
     the db lock. Then, with the lock let go of: with the same audience its
     repo is a namespace in the upstream's store and nothing is copied;
     with a narrower one its store fetches, by id, only the commits the
     upstream's branches reach (only `main` today), all readable there
     already. The landing that makes it then writes its row, its starting
     refs, and its record of the upstream, `refs/newgit/from/<upstream>` in
     its namespace (`repos::FORK_RECORD`, at an empty commit naming the
     upstream), as one entry: the fork exists whole or not at all, across a
     crash too. The refs are asked with nothing as their old value, so a
     second first write never moves what the first one started. The record
     is hidden from every Git request (`transfer.hideRefs`), so no push
     moves or deletes it. One user's own fork (`audience = "user"`) has no
     row and no landing: per-user repos have neither a row nor a landing on
     `main` for anything, so its fork is made outside the landing log too,
     and what makes it crash-safe is that its starting refs and record are
     one create-only ref transaction (nothing as their old value), so a
     repo with neither is started again on the next write and one with
     both is never started twice. Its store is never wider, because the
     definitions are read with every fork held to the upstream its
     definition names and to the recorded one (`policy::cap_forks`, run
     until nothing changes, wherever definitions are read, a landing's
     included). Against each upstream: a fork within it is left alone; one
     the upstream is within (its definition says wider, or drops `from`)
     is served as the upstream's audience; one not ordered with it
     (`user:jack` under `user:bob`) is not defined, since moving it would
     hand its own commits to readers who never had them. Capping only ever
     narrows. One whose ceiling can't be settled otherwise (an upstream not
     defined, a cycle, a linked fork whose home can't be read) is not
     defined either.
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
   whole index, checked under the lock, and in place before answering, and
   nothing rolls up. Two merges begun from one index at different anchors
   both land: the second is merged onto the first (decision 9). Who must
   approve is decided by the anchor exactly as before, read again at the
   head the merge lands on.
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
   *Landing*, items 3 and 4; `crates/newgit-serve/src/landing/`).
   - **One hidden Git repository per server**, `<data>/landing.git`: this
     server is one shard. Each landing (a project made or deleted, a
     merge's finish, a branch made, a tracker made or moved between stores)
     is one commit, written with `git fast-import`, objects and refs
     fsynced; its stamp is its place in that history (1 for the first). The
     commit's tree is the whole state, every branch's list as gitlinks
     under `<owner>/<project>/branches/<branch>/`, every tracker's store
     under `<owner>/<project>/stores/`, and the project's incarnation at
     `<owner>/<project>/incarnation`, so "state at stamp T" is the tree of
     commit T (`GET …/branches/<branch>?at=<stamp>`), never a commit date.
     Its message holds the entry: a format version, lists, every ref move
     with its old value, store moves, and the commits a move takes from a
     merger's landing. Fields added later (#411's `epoch`) are defaulted
     when an older entry is read (stored rooted histories, decision 10, are
     derived from entries and in none); an
     entry of a newer format than the server reads stops it, in place or
     not (the newest entry is checked at every start). Gitlinks name
     commits without holding them, so the log holds no content and is
     served to no one.
   - **Nothing is written down that cannot be put in place.** The check
     under the lock verifies everything putting it in place relies on: each
     project is the incarnation the landing was written for, each branch
     and store is as it expects, each ref it moves is where it says (its
     old value is recorded), and each commit a ref moves to is in the store
     it lands in. A merge naming a commit nobody sent is refused (400) and
     the landings batched with it land.
   - **A project is an incarnation.** Making a project is a landing that
     gives it a new incarnation id, recorded in its creation and in every
     entry that names it, deletion included. An entry acts only on the
     incarnation it was written for, so nothing written for a deleted
     project ever touches one made again under its name, and `?at=` on the
     new one never shows the old one's lists.
   - **Everything else follows it.** After the commit, the landing is put
     in place: trackers it moves are copied into their new stores; then,
     under the db lock, every store's `update-ref --stdin` transaction is
     prepared (every ref compare-and-swap against its recorded old value,
     full refnames across namespaces) and only once all are prepared, each
     is committed; then every row, with the stamp marked in place
     (`landings_placed`), in one SQLite transaction; then what moved is
     removed where it was, and the stamp marked finished. A reader holding
     the lock (every API request, every Git request's authorization) sees
     all of it or none. A ref someone else moved since the check (a push
     racing the landing) is left as they left it and logged, never
     overwritten; an asked ref found so is said back to the merger as
     behind. At startup, before anything is served, every entry not marked
     in place is put in place, and every one marked but not finished is
     finished; one marked is never put in place again.
   - **Group commit.** The first landing to arrive leads: it takes
     everything waiting, checks each in order against what is there and
     what the batch lands before it (lists, stores, projects, and refs),
     writes them with one `fast-import` (each its own commit), and puts
     them in place together, while the next ones queue behind it. When a
     batch cannot be put in place together, its landings are put in place
     one by one, so only the one at fault (and any after it on the same
     projects) is stuck. 2,000 landings from 100 threads, each making a
     branch (one entry, one ref move in one store, one row, no objects):
     about 280 a second in batches of 50, release build, one M5 Pro Mac
     (`landing_log.rs`, `landings_per_second`; 390 before refs and the
     landing's SQLite transaction were fsynced and each store was read for
     the compare-and-swap; nixpkgs-bench.md measured 382 for its pooled
     layout). That is the log's own cost, not a merge's:
     a real merge also places its commits, touches several refs and
     stores, and is checked against each store, so merges land slower.
   - **Roots' locks.** A landing names every project it touches in its
     entry (a linked tracker's home among them) before it waits, and takes
     all of them at once, so a tracker linked into several projects stays
     atomic with all of them. Only one batch is checked and written at a
     time. A merge's finish holds its projects from deciding which store
     each new commit goes into until it has landed, so no store move can
     land in between and leave a refused merge's commits in a store wider
     than its tracker's. A landing that waits more than ten seconds for a
     project another holds is answered 503, saying what holds it.
   - **A landing that moves storage holds its roots, not the log.** Copying
     a tracker into another store can be large, so it is put in place by
     its own request's thread after the batch is written, holding only its
     projects' locks, never the db lock or the log: other projects land
     meanwhile, and reads go on.
   - **A landing written down and not in place is stuck, loudly.** See
     *Known limits*.
   - **A push into a store is counted while it runs.** A move waits for
     every push that began where the tracker was (before its row moved)
     before removing it there, so a ref a push in flight makes or moves is
     carried to the new store, not left behind in the old one.
   - **One server per data directory.** `serve.lock` is held while one
     runs; a second refuses to start.

9. **The server merges** ([landing.md](landing.md), *What a user sees* 1–3,
   *Landing* 1–3, 5, 6, 8; `crates/newgit-serve/src/server_merge.rs`,
   `reparent.rs`, `merge.rs` `finish`). `merge` lands or it doesn't.
   - **The base is the server's.** `begin` reads the index root's base
     commit pins (`index_of`) and refuses a `base` that is not exactly that,
     and `finish` uses the base `begin` read. Every rule compares the base
     with the list, so a base the client wrote could hide a tracker the
     merge moves; nothing a decision rests on is taken from the client
     unread (the list is read from root's commit too, the moves are checked
     against what the rules hand out, the anchor is the server's).
   - **A fast-forward when Git would make one.** A branch still at the
     merge's `base` moves to the merger's own commits, ids and signatures
     kept.
   - **Re-made on the head when the changes are apart** (`reparent.rs`).
     What each side changed is a *touch*: for each tracker whose commit
     moved, the paths of its own tree that changed (pins aside) and the
     directories that went, read in process skipping equal subtrees, as
     the bytes Git stores (a name need not be UTF-8). Two touches overlap
     when, in a tracker both moved, one changed a path the other changed or
     one inside it, or one under a directory the other deleted or renamed
     away. A touch that reshapes the composed tree overlaps every other: a
     tracker added or dropped, a tracker whose own content went entirely
     (its directory in the view is gone, and Git may follow it, and the
     directories above it, as a rename), a definition changed (`access`, or
     any tracker's own `.newgit/trackers/`: a mount may have moved), or pins
     the re-make cannot write back exactly. For these only Git's merge of
     the composed trees is right. The re-make stands in for that merge, so
     `crates/newgit-serve/tests/reparent_differential.rs` holds it to Git's: generated
     cases across several trackers, each re-made tree compared with what
     `git merge-tree` makes of the same composed trees.
     When the merger's touch (base to its list) and what landed since (base
     to head) do not overlap, no merge runs: each tracker only the merger
     moved lands at the merger's own commit, each only the head moved stays,
     and each both moved (root, and the parents along the mount chain) gets
     one new commit on the head's, its tree the head's with the merger's own
     paths and pins laid on, the merger's commit its second parent, by the
     merger, with the stock message. O(depth) objects, no `merge-tree`.
   - **Otherwise one merge, composed.** The base, the head, and the
     merger's index are each composed into one view tree (every tracker at
     its mount, `newgit_core::compose::view_tree`), `git merge-tree`
     merges them (`merge.directoryRenames=true`, so a file added under a
     directory another landing renamed follows it), and the result is
     split by tracker (`newgit_core::compose::split`). A tracker both moved
     gets one merge commit, the head's commit first and the merger's
     second, as GitHub's merge button does, holding only that tracker's
     tree and pins and the stock message ``Merge `<login>` into
     <branch>``, by the merger. Refused (409) only for a real conflict,
     naming each file in trackers the merger may read and saying only
     that there are others, not how many ("one or more files in trackers
     you cannot read"); the client then merges
     `<branch>` into its work. A tracker's `main` the merger asked to move
     to its own commit moves to the commit that lands in its place, from
     where it is now, when that comes after it.
   - **One commit, one audience: the rule for content a merge carries.**
     Following a move, Git's merge can put one side's change into the
     tracker the other side moved the file to. The rule: *a server merge is
     refused (409) when following a move the other side made would put
     content into a tracker whose store does not include the store of the
     tracker the content was in*, where a store includes another only when
     it is the same or the public one (decision 1). Concretely, with Git's
     own rename detection (50% similar) on each side against the base: a
     change one side made to a file the other side moved into such a
     tracker, or a file one side added under a directory the other side
     moved there. Moves the merger made in their own commits land as they
     made them: that is their choice, made from content they could read.
     What the merge would carry along with someone's move, which nobody
     chose to widen, is refused, naming where it would have gone; bringing
     the branch in and making the move one's own is the way through.
     Re-making on a new head moves nothing between trackers (each path is
     set from the same tracker's commit), so it never needs the rule.
   - **What lands is authorized, at the head it lands on.** Each attempt
     reads the rules (maintainers, audiences, definitions on each tracker's
     `main`, membership) and checks the merger's own change against them,
     and then checks what would land (head to result) the same way: a
     tracker the merge changes that the merger may not (a file someone
     moved into it carrying the merger's edit) is refused (403), naming it.
     The rules are read under the db lock and decided without it. After a
     re-make on a new head they are read and decided again against what
     finally lands.
   - **Rechecked by path, under the roots' hold.** The merge runs with
     nothing held; then its roots are held (decision 8; a busy hold is
     waited for, within the turn budget, rather than throwing the merge
     away) and the branch, the project's epoch, and the merger's membership
     are read again. If the branch moved and what landed in between
     overlaps this merge's touch, the attempt starts again (merging onto
     the new head); if not, the merge is re-made on it as above, without
     Git's merge. The membership read here, under the recheck's db lock, is
     what the landing goes ahead on.
   - **Barriers and epochs, on `main`.** Definitions are read from `main`,
     so only there is a change to `.newgit/trackers/` (`access`'s own
     files, or any tracker's own `.newgit/trackers/`) a barrier: it holds
     its roots before it merges, so it lands on the head it merged onto, by
     itself, and its entry sets the project's epoch one higher (`epoch`,
     defaulted in older entries; `projects.epoch` in SQLite, written with
     the landing's rows). Every merge's landing names the epoch it was
     prepared under, and `check` refuses one prepared under another, so it
     starts again. A result that changes the definitions though the
     merger's own change did not (a move followed into
     `.newgit/trackers/`) starts again as a barrier.
   - **Fairness is a turn on paths, not on the project.** After three lost
     rechecks a merge takes a turn on its touch: until it lands, no other
     merge lands over those paths (one that would waits for the turn to be
     given up), and merges on other paths land beside it. A turn waits only
     for older turns on the same paths. A merge still waiting after 40 s in
     all (the client gives a request a minute) is answered 503.
   - **Nothing composed reaches a store, and nothing is left behind.**
     Composed trees cross every audience, so they are made in a scratch
     repository of the merge's own, `scratch/<random>.git` in the home,
     which borrows every store's objects (alternates), is served to no one
     (no repo name reaches it), and is removed when the merge is done, and
     at start. Only each tracker's new commit and the objects it adds over
     its parents leave it, into the merger's landing for that tracker, and
     from there into a store exactly as the merger's own commits do
     (decision 1). Commits re-made on a new head are written straight into
     the merger's landing for their tracker. An attempt that lost removes
     the landing refs of every commit it made. Each `git` process the
     server merge starts for one step (`merge-tree`, `diff-tree`, `notes`,
     the pack it copies out, `update-ref`) is stopped after 60 s (503,
     nothing landed); see *Known limits* for the reads that are not.

10. **Each tracker's rooted history is stored** ([landing.md](landing.md),
    *Performance*; `crates/newgit-serve/src/rooted.rs`). A view rooted at a
    tracker (`/<o>/<p>/<path>.git`, root's at `/<o>/<p>.git`) is served from
    its store, not composed per fetch.
    - **What is stored: the store's view, in the tracker's own store.** For
      tracker `T` in store `S`, `T`'s view composed from exactly the
      trackers mounted within it (on that branch or tag's list, in Git)
      whose store `S`'s readers may all read: `S` itself and `public`
      (`Store::includes`). Nothing in a narrower store (`user:<login>`, a
      private instance's) and no linked tracker is ever composed into it, so
      nothing is written into `S` that `S`'s readers could not fetch by id
      already. A public child's commits are fetched into `S` by id to be
      composed: public content, which any store may hold. A store never gets
      content from a narrower one, so no URL of any store hands anyone a
      commit or tree of a view they may not read.
    - **Who is served it.** The endpoint reads the plan as before (each
      branch and tag cut to the trackers the reader may read), and serves
      the stored history only when every branch's and tag's cut is exactly
      the stored set. A reader who may read more (jack, with a `user:jack`
      tracker inside root) is composed for per fetch, into the endpoint's
      own repositories as before (*Known limits*). Never one who may read
      less: whoever may read `T` reads `S`. The repo handed out is `T`'s own
      location from `served`, in the namespace `r/<T>`.
    - **Layout.** `r/<T>` holds `refs/heads/*`, `refs/tags/*` (annotated
      tags made as the composer makes them), `refs/notes/commits` (canonical
      ids), and `HEAD`. One ref outside every namespace,
      `refs/newgit/rooted/<T>`, points at a state commit: what each branch
      and tag was composed from (`T`'s commit, the readable set, the view),
      and per readable set what every commit of `T` composed to
      (`lines/<set>/map/ab/cd/<rest>`, a blob: the view commit, the children
      expanded, its canonical id) and the canonical notes. The state ref,
      every served ref, and `HEAD` move in one transaction, a
      compare-and-swap on the state ref against the state its writer read
      when it started, which also checks the tracker's own `HEAD` is still
      in `S` (`update-ref --stdin`'s `symref-verify`; `HEAD` itself by
      `symref-update`, hence Git 2.46). A writer that loses reads the state
      again: a tracker that left `S` meanwhile (its move landed) gets
      nothing written back there.
    - **Never moves back.** The state records when its lists were read: the
      project's incarnation, its *revision* (`projects.revision`, which
      SQLite triggers move with every change to a list, tag, link, store
      row, or the default branch, in that change's transaction), and the
      stamp of the last landing there. A refresh from lists read at an
      older revision than the stored state's leaves it as it is, and the
      same history read again later records the later revision, so a
      writer that read the lists in between cannot put its view back. A
      fetch brings the history up to the lists it read before upload-pack
      starts; from then on its refs only move forward, so upload-pack never
      advertises a view behind those lists
      (`crates/newgit-serve/tests/rooted_monotonic.rs`,
      `rooted_fetch_race.rs`).
    - **As of a landing.** "Tracker `T`'s rooted history as of landing
      stamp `N`" is `rooted::as_of(app, project, T, N)`: refused until `N`
      is in place, it brings the stored history up to the lists (which
      then hold `N` and all before it) and answers its refs and the `At` it
      was composed at. Since it never moves back, what it answers stays at
      least that new. A mirror export or a webhook (#414) asks it.
    - **Extending is composing only the new commits**
      (`newgit_core::compose::extend`): `rev-list` from what is wanted, not
      from what was composed last, each new commit composed onto its
      parents' recorded views, by the same code a whole composition runs.
      So the ids are the ones composing from scratch gives, which
      `crates/newgit-serve/tests/rooted_differential.rs` holds it to over
      generated landings (merges the server writes, re-made commits, split
      commits and their canonical notes, a tracker added and removed, a
      mount renamed, a hidden pin moving alone, a second branch made, moved,
      and deleted, tags, the readable set changing), checking every
      branch's and tag's tip against `compose::compose` and every commit's
      composition and every note against a composition from scratch; it
      fails when the extension is broken on purpose. A changed readable set
      (a tracker added to a list, a child moving between stores, an
      audience changed) is another set: composed whole, once.
    - **Derived data, outside the landing.** Nothing about stored histories
      is in a landing's entry, and nothing is composed while a landing holds
      anything: `land` works out from the landing alone which trackers it
      touched (pins that differ from the lists it expects, store and ref
      moves; everything, for a definitions change, a project made, or a
      void), then returns, and one background thread extends those
      trackers and the trackers they are mounted within. Entries for one
      project coalesce in its queue. A pass reads the project's lists,
      tags, definitions, and rows once, under the db lock, and lets go of
      it; every Git process (whether the store is the tracker's, the
      composing, the ref transaction) runs after, each tracker under its
      project's compose lock, let go of between chunks of a long history.
      A request right after a landing waits for that read at most
      (`rooted_lock_wait.rs`: 300 trackers, debug build, the longest of 84
      requests made while the extension ran waited 159 ms, against 3.5 s
      before). A pass that fails or panics is logged, kept in
      `Worker::failing`, and tried again with backoff (1 s, doubling, at
      most 5 minutes) without waiting for another landing; the thread goes
      on (`rooted_worker_panic.rs`). A tracker that has left every list (or
      the default branch, or its store) has its stored history removed by
      the next pass that reaches it.
      Every fetch of a rooted view compares the stored state with the lists
      it read under the db lock and composes what is missing first, so no
      reader sees a view behind what landed, whether or not the background
      got there. A landing voided, refused, or never put in place changed no
      list, so nothing was composed for it; a crash mid-extension leaves
      objects nothing names and the state ref where it was.
    - **A data directory from before: backfilled at start, in the
      background.** Startup does not wait for it (a home of 22k trackers
      must not wait for 22k histories), and a fetch before it reaches a
      tracker composes that one itself. Long histories are recorded every
      5,000 commits (`NEWGIT_SERVE_ROOTED_CHUNK`), so a pass stopped part
      way resumes where it was (`serve_rooted_stored.rs`).
      Backfilling holds a project's compose lock one chunk at a time, so a
      fetch of another view there waits for at most one chunk.
      `NEWGIT_SERVE_ROOTED=off` composes every view per fetch as before, and
      `NEWGIT_SERVE_ROOTED_BACKGROUND=off` leaves extending to fetches;
      both are for the tests.
    - **What it costs.** Nothing inside a landing's hold: the landing path
      gains one queue push after `land` returns, no process and no object.
      The rest, from `crates/newgit-serve/tests/rooted_bench.rs` (ignored by
      default), release build, one M5 Pro Mac, 2,000 trackers mounted flat
      under `pkgs/` in root, each with one to four commits of its own:

      | | |
      |---|---|
      | backfill, each tracker's | 128 ms (spawn-bound: about 15 `git` processes) |
      | backfill, root's | 0.6 s |
      | per landing (one tracker and root's pin), the tracker's extension | p50 110 ms, p99 145 ms |
      | per landing, root's extension | p50 755 ms, p99 951 ms (O(trackers): every child grafted) |
      | store growth per landing | 0.12 MB (state, map, and view objects) |
      | a fetch's check when nothing is missing | 15–20 ms |
      | clone of one tracker's stored view | 40 ms, 1.7 KB of objects |
      | composing that view from scratch, as each fetch after a landing did | 47 ms (its history is 1–4 commits) |
      | composing root's view from scratch, the same | 9.9 s |

      Composing grafted children one at a time, each rewriting the
      directory they share, so a first run cost root 2.5 s and 32 MB a
      landing; children whose mounts are apart are now grafted in one pass
      (`Objects::set_paths`, held to the one-at-a-time result by
      `crates/newgit-core/tests/set_paths.rs`). Backfilling 22k trackers at
      128 ms each is about 47 minutes in the background, serving
      meanwhile.

11. **Mirrors: the webhook lands, exports follow** ([#414];
    `crates/newgit-serve/src/mirror/`). A mirror is a GitHub repository an
    export target names, and an audience: whoever can read it.
    - **Configuration, read at startup, never committed:** `--github
      <file>` (refused unless only its owner may read it), a TOML file
      with `api` (GitHub's REST API, `https://api.github.com` by default)
      and per project `[projects."<owner>/<name>"]` a `webhook_secret` and
      an optional `token`. The Worker has one `GITHUB_WEBHOOK_SECRET` for
      the whole remote and no GitHub credential at all (it fetches public
      mirrors anonymously, asks GitHub's API unauthenticated, and the
      client exports); here the secret is per project, so one project's
      secret cannot sign another's events, and the token (a fine-grained
      personal access token, or a GitHub App's installation token, with
      contents read and write on the mirrors) fetches, pushes, and asks
      the API. It reaches Git only through its environment
      (`GIT_CONFIG_*`, an `http.extraHeader`), never its arguments, and is
      cut out of any message that might echo it. Minting short-lived
      installation tokens from a GitHub App key is not built: an
      installation token placed in the file works until it expires.
    - **The webhook** (`POST /v1/webhooks/github?project=<owner>/<name>`)
      verifies `X-Hub-Signature-256` under the project's secret in
      constant time (503 with no secret, 401 unsigned or wrong), answers
      `ping`, records a merged `pull_request`'s head under its merge
      commit, and queues a `push` to an export target's branch (403 from a
      repository no definition on `main` exports to, or pushed by someone
      who is not a member able to merge root). A push already queued for
      that commit, or that delivery, is not queued twice; a delivery whose
      pull has ended is queued again, which is how a redelivery asks
      again. The queue is rows in SQLite, so a restart resumes it (#353).
      Tags pushed to a mirror are not brought in yet.
    - **A pull lands as a merge.** The pull thread fetches the mirror into
      a scratch repository, composes `main` as the target sees it, and
      walks what the mirror has beyond it (nothing: our own export, ends
      `nothing`). The commits are brought in with the withheld trackers'
      files put back from their parents (`compose::bring_in`, #401's rule
      included), split by tracker, their new objects put in the pusher's
      landing, and landed by `merge::begin` and `merge::finish` as the
      pusher, from the root commit on `main` the mirror's history left:
      a fast-forward when nothing landed since, the work re-made on what
      did when it changed other paths, Git's merge over the composed tree
      otherwise. So #451's first case (a branch rebuilt from `main`
      changing `crates/newgit-serve`, which is the `serve` tracker) is the
      server's merge, not a merge of views, and is refused only for a real
      conflict. Each child's `main` follows, as a member's merge asks. A
      target rooted at a tracker lands in that tracker, from the root
      commit that pinned it where the mirror's history left it, each
      tracker above it given one commit moving its pin: one landing.
    - **Whose work, by content (#404, #451).** For each instance with a
      fork of a withheld tracker holding commits `main` does not have, its
      fork of the target's tracker is composed as the target sees it, in
      the same composition as `main`. A commit the pull would land that
      *is* one of those (an exported instance branch merged with a merge
      commit), or whose pull request's head is one (a squash or rebase:
      read from the `pull_request` event, or asked of GitHub), is refused
      when that instance commit pins a withheld tracker at work `main`
      does not have, naming the instance, the tracker, and both ways on.
      Nothing is read from a branch's name, so a branch named like an
      instance whose commits are not its lands. Commits a merge commit in
      the walk brought in need no question; only new ids on the line of
      first parents are asked about, at most 20 a pull. While GitHub
      cannot answer, or lists nothing for up to two minutes, the pull
      waits, holding nothing and spending no attempt, and `newgit status`
      says so (#398). An instance's work not yet pushed to this server
      cannot be seen, and a commit carrying it lands as a commit of
      nobody's: the check is what the server holds.
    - **Ends, said where `newgit status` reads them.** `landed`,
      `nothing`, `refused` (with why and what to do), `gave up` (after 10
      failed attempts, 15 s doubling to 10 minutes, or two hours). A pull
      refused only because it carries an earlier refused pull's commits is
      refused with it and said once, on the first, with how many wait on
      it (#451's third case). Trouble stays said until a pull of that
      target lands or finds nothing.
    - **Exports are derived data, the one thing that may be behind.** A
      landing only counts itself for the export thread (no work in its
      hold). The thread composes each GitHub target's view (the history
      rooted at its declaring tracker, only the trackers it ships, a pin
      expanded only when it is in that tracker's own history: the ids the
      client's `newgit export` makes) and pushes it, never forced. Where
      the target withholds nothing and ships exactly what the declaring
      tracker's stored rooted history is composed from, that history's tip
      is pushed (`rooted::as_of`); otherwise the view is composed per
      export, O(history) (*Known limits*). Before every push, every commit
      it adds is checked to hold nothing at the mount of a withheld tracker
      while that tracker is pinned there, and nothing a tracker outside
      Git owns; failing that, nothing is pushed. A mirror with commits
      `main` lacks is left as it is: the export waits for the pull, or
      says how to bring them in. Failures retry with backoff; `/state`
      says per target `up to date`, `behind` (landings since), or
      `failing` with the reason, and `newgit status` says the last two.
      A pull's commits were split with their originals in their trailers,
      so they compose back to themselves: a pull then an export is a
      fast-forward on GitHub.

## Known limits
- **A view for a reader who reads more than its store's audience is
  composed per fetch.** A member who may also read a private tracker
  mounted inside the view gets it composed on each fetch after a landing,
  O(history), as every view was before decision 10. One history per
  distinct readable set would be 2^k for k private audiences; composing
  them from cached subtrees is #440.
- **Extending a parent of many trackers is O(trackers) per commit.**
  Composing one commit grafts every child at its mount, so root's
  extension at each landing costs what composing one commit of root's view
  costs (decision 10, *What it costs*: p50 755 ms at 2,000 trackers), in
  the background and outside the landing, but a fetch of root's view right
  after a landing waits for it. The queue is deduplicated, but at a landing
  rate above one per extension the backlog in time grows without limit.
  The nixpkgs prototype's pooled mixed-reader layout was 23–32 ms a
  landing, flat, because it memoized by subtree and redid only the paths
  that changed; reusing the previous commit's subtrees for the children a
  landing did not touch is the fix, recorded on #440 with these numbers.
- **A pull composes `main` as its target sees it, whole, and an export
  that cannot use a stored history does too.** O(history × trackers) per
  pull and per export of a target that withholds something (newgit's own
  `github` target withholds `t3code`). In the background, outside every
  hold, but a long history makes mirrors slow to follow. Keeping each
  target's composition between runs (as decision 10 does for readable
  sets) is the fix, with #440.
- **What stored histories leave behind stays until gc ([#434]).** Each
  extension writes a new state commit and map trees, and canonical notes
  are kept for view commits a branch no longer reaches. (A tracker moving
  to another store has its `r/<T>` and state ref removed from the old one
  with its namespace, and an extension still running there when the move
  lands writes nothing back: decision 10, *Layout*.)
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
- **A narrower fork's start is checked, then copied.** Whether the fork
  is within its upstream and whether the caller may read the upstream are
  checked under the db lock, and the upstream's commits are fetched into
  the narrower store by id after it is let go of (`ForkStart::place`). An
  upstream narrowed, or a caller who loses access, in between (or a
  landing that then fails) leaves those commits in the narrower store,
  whose readers could read them at check time. Re-checking under the
  landing needs the landing to take a precondition on another tracker's
  facts, which it does not yet.
- **One log per server.** Moving a set of homes to another node means
  splitting the log, which nothing does yet. A landing that touches a
  linked tracker on another server is two landings (landing.md), each in
  its own server's log.
- **A push that runs past ten minutes during a move** is no longer waited
  for: what it leaves in the old store stays there, and is logged. While
  the move waits for it, the merger whose landing moves the tracker waits
  too, and every other landing on that project is answered 503 after ten
  seconds, naming the push it waits for.
- **A stuck landing blocks its projects until it is put in place or
  voided.** A landing written down that cannot be put in place (a store
  unwritable, a disk full) is answered 503 with the real cause, which the
  server's log says too. Every later landing on its projects is refused
  with the same 503 after trying it again first, and so is every start of
  the server. Other projects land as before. It also holds back the
  applied mark, so `?at=` refuses every stamp after it until it is
  resolved. The way out: fix the cause and restart (or land again there),
  or, when the cause cannot be fixed, stop the server and run `newgit
  serve --data <dir> --void-landing <stamp>`. That appends one
  compensating landing (the log is never edited) which gives up that
  landing and every one stuck after it on the same projects: their
  projects are set to what SQLite has, which they never changed (rows
  SQLite has that the void does not name are removed); refs they moved
  are moved back; a tracker they were copying toward another store is
  removed from it. Once the void is written down, the landings it names
  are never put in place: a restart or a retry reads every entry first,
  and a landing a later entry voids is skipped, even when putting the
  void itself in place failed or was interrupted. The void is then stuck
  like any landing, tried again at each landing on its projects and at
  each start, and putting it in place again is safe. A void and a landing
  already voided cannot be voided. **The objects copied stay in that
  store until its garbage collection, which does not exist yet ([#434]),
  and under protocol v2 its readers can fetch one by id: voiding a stuck
  widening leaves its history readable by the wider audience.** Voiding
  says which stamps it gave up; fixing the cause and retrying leaves
  nothing behind.
- **A landing in place and not finished has landed, and is never
  voided.** A store move whose rows are written is in place: readers see
  it, through the API and through Git. If removing the tracker from the
  store it left then fails, its merger is answered 200 with `pending`
  saying so, not 503, and the landing is finished (never voided) by
  trying again, which every later landing on its projects does first, as
  does every start. Until then those landings are refused with a 503 that
  says it is in place and what failed. `--void-landing` refuses it: moving
  its refs back while its rows stay would split the API from Git.
- **Concurrent merges onto one branch take turns at the hold.** Each
  attempt merges outside the lock, but landings on one project are held one
  at a time, and what each does while held (the recheck, re-making the
  merge on a head that moved, reading the rules again, placing its commits,
  writing the landing) is a few hundred milliseconds of `git` in a debug
  build. Measured with `serve_server_merge_bench.rs` (ignored by default;
  `BENCH_MODE=<mode>`): 24 merges begun from one base, finished at once
  from 24 threads, debug build, one M5 Pro Mac, landed per second (all 24
  land in every mode):

  | mode | what each merge changes | before | after | Git merges run |
  |------|-------------------------|--------|-------|----------------|
  | `mixed` | a new file in `blog` or in root, alternately | 1.0 | 2.8 | 0 |
  | `same` | a new file in `blog` | 0.9 | 2.1 | 0 |
  | `spread` | a new file in its own tracker (27 in all) | 0.25, 8–9 answered 503 | 2.1 | 0 |
  | `overlap` | its own line of one file in root | — | 1.0 | 185 |

  "Before" is the whole-branch recheck this replaced, as measured in the
  review of #436. The three modes whose changes are apart now cost the same
  per landing whatever the number of trackers (`mixed` is quicker because
  half its merges move one tracker, not two). Merges whose changes overlap
  run Git's merge again each time something lands on their paths first,
  and composing their views is O(trackers) (#440).
- **A member removed after the recheck** (between the recheck's db lock,
  where membership is read for the last time, and the landing's commit,
  which spans placing the merge's commits) is not caught: membership is a
  row, not a landing, so it moves no epoch. A maintainer removed is: that
  is a change to the definitions.
- **A linked tracker both sides moved is refused** (409), naming it: its
  history is at its home, which this merge does not land in. Land it there
  first.
- **A tracker that gains content only by a merge** (a file the other side
  moved into it while this side edited it) gets a merge commit whose
  second parent may be behind its first: split makes one commit per
  tracker whose merged tree is neither side's, on both sides' commits.
- **Not every `git` a merge waits on is timed.** The long-lived
  `cat-file --batch` that reads objects in process (`Objects`: touches,
  the re-make, composing), and the authorization's ancestry checks
  (`merge-base --is-ancestor`), `cat-file -e`, and `diff-tree` reads run
  without the 60 s limit. Each is a bounded read of objects the server
  holds, so a hang means a broken repository rather than a slow merge, but
  one would hold the merge's turn, or its roots, until it ends.
- **Merges land at about two a second** (a debug build), though a merge
  apart from what landed runs no Git merge: each spawns about 55 `git`
  processes, and the re-make, the rules check, placement, and the landing
  run one after another while the roots are held, about 400 ms a landing.
  Issue #445 has the counts and the way to milliseconds (objects written
  in process, the object format cached, re-made objects written straight
  into the store).
- **A merge's finish holds its projects while it places its commits.**
  From deciding which store each new commit goes into until it has
  landed, a merge's finish holds its projects' locks, and that includes
  copying its commits from the merger's landing into those stores. A
  large placement therefore answers every other landing on those projects
  503 after ten seconds, saying a merge is placing its commits. Other
  projects, and reads, are not held.
- **`?at=<stamp>`** answers the stamp's own landing, read from one
  resolved log commit. It refuses a stamp not yet in place (409, every
  landing up to the applied mark is), a project state that shows a landing
  voided after it (410), and answers nothing for a stamp from a deleted
  project of the same name.
- **What survives power loss.** The log's objects and refs, every store's
  objects and refs (`core.fsync=committed,reference` and
  `core.fsyncMethod=fsync` on the log and on every store, written into a
  store made before this when the server starts, so a push, which runs
  with the store's config, gets them too; and passed on every Git command
  the server runs), and SQLite's landing transaction (`synchronous=FULL`
  for it) are synced, in that order. On macOS a plain `fsync` leaves
  writes in the drive's cache; Git's `fsync` method flushes it
  (`F_FULLFSYNC`), and SQLite does too (`PRAGMA fullfsync`,
  `checkpoint_fullfsync`). The order: a landing's log commit before
  anything follows it, its refs before its rows, its rows (and its mark)
  before its answer. So a landing whose merger was answered
  survives, a landing marked in place is in place, and one written down
  and not marked is put in place at the next start. Other SQLite writes
  (tokens, members, instances) run `synchronous=NORMAL` and may lose their
  last moments; a push in flight may lose its ref update, as Git's own
  would.
- **A damaged log refuses to start.** The log is made only when
  `landing.git` did not exist before (a data directory from before the
  log, whose SQLite says no landing was put in place). A log that cannot
  be read, one missing while SQLite says landings were put in place from
  it, and one that ends before SQLite's applied mark all stop the server
  with a message saying so, never a silent fresh start: restore
  `landing.git` from a backup.
- **`?at=` and startup cost grow with the log.** `?at=` resolves
  `<tip>~<n>`, which walks back `n` commits, and the first start of a data
  directory reads every entry after the applied mark; neither is indexed
  yet, and the log is never compacted ([#434]).
- **Pushes that move no tracker's branch are not landings**: a fork's, a
  seed's, a per-user repo's, a tracker outside Git's. They move their own
  refs, in their own store, as before.

[#411]: https://github.com/Spheroman/newgit/issues/411
[#434]: https://github.com/Spheroman/newgit/issues/434

## Not yet

- Garbage collection of the stores, of merge landings, and of the landing
  log ([#434]).

- Tags made on a mirror, brought in by the webhook (the Worker does).
- Minting GitHub App installation tokens (decision 11).
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
