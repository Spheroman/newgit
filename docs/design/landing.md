# Landing

**Status: decided; step 1 of the build order is built.** How changes land in a tree of trackers on
`newgit serve`. It replaces [anchoring.md](anchoring.md), which describes how
the Worker on newgit.dev lands merges today and goes when newgit.dev moves to
`serve`. Settled with Jack on 2026-11-10 after two adversarial reviews and a
nixpkgs-scale benchmark ([nixpkgs-bench.md](../research/nixpkgs-bench.md)).

The standard it was held to: **server complexity is fine; user-visible
complexity is not.** Any rule a user or agent has to learn, rather than one
they'd assume from Git, is a defect. Performance is the other constraint: a
domain the size of nixpkgs (about 22k packages as trackers) and hundreds of
agents merging at once.

---

## How we got here

v0.1.0 kept one store with one history, and trackers partitioned paths
("repos are an output of the store, not the storage unit"). #117 gave every
tracker its own repo, because Artifacts tokens are repo-scoped, so a
permission boundary had to be a repo boundary. Everything in `anchoring.md`
followed from working around that on Durable Objects: landing at the lowest
tracker that covers a change, deferred roll-ups, inboxes and outboxes,
reservations, and the "pending" and "behind" states users could see.

On one server, none of that is needed. A landing is a few small objects and
one transaction; Scion moves `main` with "a compare-and-swap on a single
Spanner row" at 30,000 merges a day, and that's the shape here too. What
the old design called *deferred* is fine as an implementation detail, as
long as no user can ever observe it. Anthropic's Scion calls this *derived
data*: computed early or late as a performance choice, never visible as a
state.

## What a user sees

1. **`merge` lands or it doesn't.** When it returns, every view on the
   server shows it. "Pending" only ever means "not landed yet" (approval,
   checks). Only something on another server can be "behind": a mirror, a
   dependency trailing its home, or your own workspace you haven't pulled.
2. **You're refused only for a real conflict, and the file is named.** It's
   Git's own merge over the composed tree, so a file moved between trackers
   is followed as in a monorepo.
3. **`git push` semantics.** If nobody landed since your base, `main`
   fast-forwards to your exact commits, ids and signatures intact. If someone
   did, the server writes one merge commit with your commits as its second
   parent, as GitHub's merge button does.
4. **Your commits come back as themselves**, wherever you rooted your
   workspace. A split keeps the original commit, and composing gives it back.
5. **Editing something on another server is two landings, in order.** You
   edit `dots/` or upstream t3code in place, like any file. `merge` lands
   that part at its home first, then your pin move, and says so: `landed in
   spheroman/dots, then in spheroman/newgit`. If you can't land there, it
   opens a proposal and waits: `waiting on spheroman/dots: proposal #12`.
   A partial result (theirs landed, yours conflicted) is reported plainly,
   and running `merge` again resumes it.
6. **Plain `git push` works.** The server splits a pushed commit by tracker.
7. **One commit, one audience.** A commit spanning trackers with different
   readers is refused, so no message leaks into a wider tracker.
8. **A profile is a directory of repositories.** `newgit clone spheroman`
   clones each project; `newgit log` at a profile interleaves them by landing
   time. This departs from #201's "one `git log` at the profile", which would
   either invent commits or make every merge in every project wait on one
   profile.

## Trackers, storage, and mounts

- **Every tracker is its own Git history, always.** There is one kind of
  tracker: paths, audience, maintainers, resources, an owner, and a mount.
  Storage never changes shape when a tracker is carved, linked, given to
  someone else, or published on its own.
- **Ownership is policy, never storage.** Giving `lib` to someone else
  changes who approves it, who reads it, and where it exports. It stays
  mounted at `lib/`, with the same history, and a change across `lib` and the
  rest of the repo is still one atomic merge, needing both sets of approvals.
- **Storage: one object store per audience, every tracker a ref namespace in
  it.** Never one repo per tracker (22k repos: 424k files, a 57 s tree-wide
  change, 143 s gc), and **never a store shared across audiences**: under
  protocol v2 any object in a repo can be fetched by id, a `have` line
  reveals that a hidden commit exists, and a crafted push can point a ref at
  any object in the store. A check in front of upload-pack can't close all
  three without reimplementing Git's authorization, which would be
  simulating security. An embargoed branch gets a store of its own. Built
  (#409): [serve.md](serve.md), *The shape* and decision 1.
- **Mounts** say how a tracker sits in its parent, the same for every tracker:

  | Mount | The child's head | The parent records | Example |
  |---|---|---|---|
  | inside (default) | follows the child | every move, in the same landing | `t3code` in `newgit` |
  | dependency | the child's own, at its home | a pin, moved by a commit | `pinned = true`, or a tracker on another server |
  | beside | the child's own | nothing | projects under a profile |

## Landing

On one server:

1. **Merge outside any lock:** compose base, head, and the change; run Git's
   merge (equal subtrees are skipped, so the cost follows the change, not the
   tree); split the result by tracker, reusing original commits.
2. **Read policy at the head it will land on**: maintainers of what the
   change touches, audiences, definitions. Never at the change's base: a
   maintainer removed in between must not land work.
3. **Under the lock** of each root the change touches, taken in a fixed
   order: recheck that nothing on the same paths landed since step 1, then
   write one commit to the server's landing log naming every touched
   tracker's new head, and move the refs. Concurrent landings are batched
   (group commit), each still its own entry.
4. **The landing log is the commit point.** It's a hidden Git repository per
   server shard; its sequence number is the landing stamp. Repo refs and
   SQLite follow it and are repaired from it at startup, and readers resolve
   heads through it, so no reader sees half a landing. "State at T" reads
   the stamps, never commit dates.
5. **Barriers:** a change under `.newgit/trackers/` (definitions, audiences,
   mounts) lands only on the current head, by itself, and bumps an epoch. A
   landing prepared under an older epoch starts again.
6. **Fairness:** after a few lost rechecks, a wide change gets a turn to
   itself.
7. **Conflicts are per file by default, with CI after merge**, as Anthropic
   runs at agent scale. A tracker can opt into strict landing through a merge
   queue with speculative batches.
8. A directory renamed by a landing reruns the merge of any landing prepared
   before it, so a file added under the old name follows the rename.

Across servers there is no transaction: two landings, in order (rule 5).

## Secrets and trackers outside Git

Unchanged in shape: their own history, never in the workspace's `.git`, never
in a view, never exported. On `serve` they are **encrypted to their audience**
(#338) with age and the members' GitHub SSH keys, unless `public` (built: see
the reference's *Encrypted trackers*). The server stores ciphertext and
cannot read it, but it is still trusted for four things: it keeps the
member list, so it could add a login whose keys then become recipients
(every change to the set is printed by fingerprint); it serves the tracker
definitions, which decide whether a path is encrypted at all (a machine that
has held a path's encrypted content refuses to store it unencrypted until
`newgit tracker allow-plaintext <path>` there, a confirmation rather than a
boundary; a machine that never held it has nothing to compare against);
nothing is signed, so it could substitute content that decrypts cleanly
(#431); and names, sizes, and commit metadata stay readable to it. They merge on the client, which can
decrypt; the server still checks ancestry and moves refs in the same landing
transaction. A checkpoint records their revisions in the binding record, not
as Git parents, so no shared store references a private commit.

## Instances and checkpoints

One ref per live instance, pointing at a commit whose parents are its
trackers' tips: a garbage-collection root for unlanded work, hundreds of refs
rather than instances × trackers. A checkpoint is that commit plus resource
state. Undoing a change landed across two servers is two reverts in reverse
order.

## Performance

From [nixpkgs-bench.md](../research/nixpkgs-bench.md) (22,287 packages, one
Mac, relative numbers):

- Spawning one package rooted at it: 0.13 s and 22 KB from the pool, the
  same as a repo of its own. Without a stored rooted history, a partial
  clone of one package took 99 s and 1.6 GB, so **each tracker's rooted
  history is always stored**, never computed per fetch.
- Landings: about 380 per second batched, against about 290 for one repo
  with stored splits.
- `git log` on one package: 6–17 ms.

Requirements that follow:

- Pins and definitions are nested trees following mount paths, so a change
  rewrites one tree per directory level. Ownership is resolved through a path
  trie cached by `access`'s tree id.
- Views per reader are cached per subtree and composed incrementally; the
  public and full views are composed ahead of time. Distinct readable sets
  are capped and measured (k private audiences can mean 2^k histories).
- Refs in reftable or SQLite, not loose files.
- Agents spawn rooted at the tracker they work on, by default.
- At company scale the same model holds, with objects in a key-value store
  (same Git objects and hashes), a per-machine daemon fetching trees and
  blobs through cache tiers, and history answered from the server's indexes.

## Deleted

Anchors as a concept, deferred roll-ups, inboxes and outboxes, reservations,
`pending` and `behind` for anything on the same server, and `history =
"pins"`. Composing and splitting stay, uniform for every tracker.

## Build order on `newgit serve`

1. **Done (#409).** Pool per audience: trackers as ref namespaces, refs in
   reftable, one audience per store enforced at creation.
2. The landing log and one-transaction landings with group commit; readers
   resolve heads through it; startup repair.
3. Server-side merge (compose, `merge-tree`, split) in place of "merge
   again", with barriers, epochs, and fairness.
4. Stored rooted histories, extended at landing.
5. Dependencies across servers: ordered two-step landings, proposals, resume.
6. ~~Encryption of trackers outside Git (#338).~~ Done on the client; the
   Worker and `serve` move the ciphertext as opaque blobs.
7. Move newgit.dev from the Worker to `serve`, and delete `anchoring.md`.

## Open

- Whether a large org needs content-only privacy inside one store
  (same ids for everyone; hidden trees and blobs refused; commit messages
  leak), as Scion and Piper do, instead of a store per audience.
- Many concurrent clones against one pool, conflicting landings, and
  readers with thousands of refs were not measured.
- How a profile's listing avoids leaking that a tracker exists (#201,
  "the registry leaks shape").
