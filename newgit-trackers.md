# Trackers all the way down

*Design for newgit's core model: no privileged Git source. Every tracker is
its own repository; a workspace is composed from the trackers it binds; Git
and GitHub see a projection. Supersedes the v1 split between "source" and
"other trackers". Decisions marked **(decided)** are made; **(open)** ones
are listed at the end.*

---

## The model in one page

- **A tracker** is a definition in `.newgit/trackers/<name>.toml`: the paths
  it owns, who may read it, which resources it brings, and which role (if
  any) it fills.
- **A tracker's repo** is its history: an ordinary Git repository with a
  `main` branch (what new instances get) and one branch per instance that
  has changed it. On the remote, it is that tracker's Artifacts repo.
  *("Lane" is retired as a term: it is just the tracker's repo.)*
- **A workspace** is composed from the trackers an instance binds: each
  tracker's files at the instance's commit, laid out by path. To agents and
  tools it is one ordinary Git repository.
- **A resource** is brought by a tracker and runs what that tracker's content
  needs (a database, a dev server, an install).
- **The project** is `.newgit/` itself — definitions only — held by the one
  tracker every instance binds, `project`.

```
            trackers (each its own repo: main + instance branches)
   project ─┐   root ─┐   web ─┐   electron ─┐   local-db ─┐   jack-env ─┐
            ▼         ▼        ▼             ▼             ▼             ▼
         ┌───────────────────────────────────────────────────────────────┐
 bind →  │ workspace: files composed by path · .git = projection          │
         │ resources: those the bound trackers bring                      │
         └───────────────────────────────────────────────────────────────┘
            ▲ capture splits commits back into each tracker's repo by path
```

---

## Tracker definitions

```toml
# .newgit/trackers/electron.toml
paths = ["apps/electron"]
audience = "project-devs"        # public | project-devs | user | user:<login>
priority = -1                    # negative: bound only when asked
resources = ["electron-dev"]

# .newgit/trackers/local-db.toml
role = "db"                      # trackers sharing a role are alternatives
paths = [".env.db"]
resources = ["postgres"]

# .newgit/trackers/staging-db.toml
role = "db"
paths = [".env.db"]
priority = 10
```

| Field | Meaning |
|---|---|
| `paths` | What it owns. The most specific owner of a path wins, so `root` can own `/` while `electron` owns `apps/electron`. |
| `audience` | Who may read its repo. Enforced by the remote (token minting). |
| `role` | Trackers with the same role are mutually exclusive alternatives: an instance binds exactly one per role. A tracker without a role is its own slot. |
| `priority` | Which alternative is picked by default: highest readable wins. **Negative: never picked automatically** — only by `--use` or a personal setting. |
| `resources` | Resources this tracker brings. A resource is bound iff a bound tracker brings it. |
| `requires` | Trackers or roles this one needs: bound with it, and merged before (or with) it. |
| `merge` | `with-change` (default: merged together with whatever else a change touched) or `explicit` (only by `newgit tracker merge`; env files). Replaces `merge_with_source`. |
| `secret` | Never in the workspace's Git: excluded, never committed, captured by newgit from the files at checkpoint. Env trackers set it. |
| `production` | Production content: agent and instance tokens may not bind it, and its resources' destructive hooks are refused. |

**Ownership (decided).** Every path in a workspace has exactly one owner:
the most specific path among the *bound* trackers. Two bound trackers may not
claim the same path unless one's claim is strictly more specific. Trackers in
the same role may claim identical paths — they are never bound together.

**The root tracker (decided).** `newgit init` creates `root`, owning `/`. It
holds whatever no other tracker claims — in a fresh project, everything — so
a project never has unowned files. Carving out a tracker (`newgit tracker
create electron --paths apps/electron`) moves those paths' history out of
`root` into the new repo (a path-filtered split, done once).

---

## Binding

At spawn, for each slot (each role, and each role-less tracker):

1. Your explicit choices first: `--use <tracker>` and
   `.newgit/local/settings.toml` (`use = [...]`, machine-local, uncommitted).
2. Otherwise the highest-priority tracker in the slot you may read, if its
   priority is ≥ 0. Two readable alternatives at equal priority is a
   definition error naming both.
3. Then the resources the bound trackers bring.

`newgit tracker use <tracker>` binds a tracker into an existing instance (swapping
out whatever filled its slot); `newgit tracker drop <tracker>` unbinds a
role-less one. Swapping out captures the outgoing tracker's changes to its own
branch first, stops (never cleans up) resources no longer brought, and starts
the ones newly brought.

**Dependencies (decided).** `depends_on` names a resource or a role. A role
dependency is satisfied when the role is filled: if the bound tracker brought
resources, the dependent waits for them; if it brought none (`staging-db`),
its content alone satisfies it. An unfilled role is a spawn error naming the
role and the trackers that could fill it.

---

## The workspace is a projection

Agents expect one Git repository, and newgit lies to them about it, on
purpose, without emulating Git: the workspace's `.git` is a real repository
whose commits newgit composes.

**Composing (decided).** A composed commit's tree is the bound trackers'
trees, each grafted at its paths. Its message lists the tracker commits it
was composed from (`Newgit-Tracker: web 3f2a…` trailers); identity and date
are fixed, so composing the same tracker commits gives the same composed
commit on any machine. The workspace's history is one composed commit per
state the instance has been in: spawn, each merge brought in, each capture.
`git log`, `git diff`, `git blame` (within a state) all work; per-tracker
history is `newgit log <tracker>`.

**Splitting (decided).** At capture (checkpoint, push), every workspace
commit since the last capture is split by ownership into one commit per
tracker it touched, on that tracker's instance branch — same message,
author, and date, plus a `Newgit-Split: <workspace commit>` trailer. The
workspace commit is recorded as split (a note), so it is never split twice.

**Writes outside the binding (decided).** A change to a path whose owner is
not bound (an agent creates `apps/electron/x` in an instance without
`electron`) refuses the capture and says so: `apps/electron belongs to
electron, which this instance has not bound — newgit tracker use electron`.

**Rendered values** (ports written into files by resources) are reversed
before splitting, as capture does today, so no tracker's history ever
records one instance's ports.

---

## Checkpoints, merges, and coherence

**A checkpoint** names one commit per bound tracker (plus resource state). It
is the atomic unit across trackers: spawning or undoing a checkpoint restores
every tracker to the commit it names.

**Merges stack by tracker (decided).** Trackers declare what they require:

```toml
# .newgit/trackers/electron.toml
paths = ["apps/electron"]
requires = ["supabase"]      # trackers or roles; a client assumes its backend
```

From an instance's changes, you choose which trackers' changes to merge
(`newgit merge <instance> --only supabase`); the rest are left on the
instance's branches, not lost. A choice is valid when, for every tracker it
merges, every tracker it requires *that also changed in this instance* is
merged too:

- `supabase` alone — fine; the electron changes stay unmerged.
- `supabase` and `electron` — fine; `main`s move in requirement order, all or
  nothing.
- `electron` alone while `supabase` also changed — refused, naming the
  requirement. If `supabase` did not change here, `electron` merges alone:
  its requirement is already met by `main`.

Commits are split per tracker, so a commit that touched both does not pin
them together: the requirement graph, not commit boundaries, decides what may
be separated. An instance spawned from another (`spawn frontend --from
backend`) stacks the same way — its changes merge after, or with, the ones it
was built on. Every merge moves the affected `main`s only after all of them
succeeded: the remote's ProjectDO serializes merges and flips them together.

**`requires` governs binding too.** An instance cannot bind `electron`
without `supabase` (or, for a role, some tracker filling it): `newgit tracker
use electron` brings in what it requires, and dropping a tracker others
require is refused, naming them. Trackers have `requires` (trackers or
roles); resources have `depends_on` (resources or roles) — one model.

`explicit` trackers (env files) are never carried by `newgit merge`; they move
only by `newgit tracker merge <tracker>`.

---

## On the remote

| Repo | Holds | Read | Write |
|---|---|---|---|
| `<p>.t.<tracker>` | a shared tracker's `main` | its audience | the merge executor |
| `<p>.t.<tracker>.<login>` | a per-user tracker, all branches | that user | that user |
| `<p>.i.<instance>.<tracker>` | an instance's branch of one shared tracker | members | the instance |
| `<p>.composed` | the composed projection of every shared tracker's `main` | members | the merge executor |

**Instance writes (decided).** Artifacts has no branch permissions, so an
instance never writes a shared tracker's repo: it writes a fork of it, made
**lazily** — only for trackers the instance actually changes, at first push.
Most instances change one or two trackers, so most cost one or two forks.
Per-user trackers are written directly; their repo is the user's own.

**`origin` (decided).** A workspace's `origin` fetches `<p>.composed`, so `git
pull` shows trunk as one repository. Per-user content is never in it; the
workspace composes the user's own on top.

---

## Git and GitHub

**GitHub is an export (decided).** `newgit export --github <repo>` pushes a
composed projection of the `public` (or chosen) trackers' `main`s, with
composed history, to an ordinary GitHub repository. GitHub never holds a
tracker the export did not include, so `.env` files can live in the project
and never reach a public mirror.

**Adopting an existing repository (decided).** `newgit init` in a Git
repository imports its full history as `root`'s history. Trackers carved out
later take their paths' history with them.

---

## Build order

Each step lands alone and keeps the suite green.

1. **Tracker repos with history.** Lanes become real branches (`main`,
   `instances/<instance>`) with parents, messages, authors. Capture commits;
   merge merges. The env-file case keeps working unchanged in behavior.
2. **`root` tracker; ownership by specificity; `secret`.** *(Landed.)*
   `newgit init` writes `trackers/root.toml` (owns `.`, committed) and
   imports the project's history as root's `main`, borrowing its objects.
   Trackers nest; the most specific owner wins. `secret` defaults to true,
   and until step 3 only root may be committed (`secret = false`). The
   `project` tracker moved to step 3: until workspaces are composed,
   `.newgit/` lives in the project's own history, and a second copy would
   have nothing reading it.
3. **The projection, and the `project` tracker.** Composed workspace commits, splitting at capture, the
   unbound-owner refusal, `newgit log <tracker>`.
4. **Roles, priority, resources, use/drop.** Binding per slot; resources
   brought by trackers; role dependencies.
5. **Coherent merges** locally and on the remote (lazy per-tracker forks,
   ProjectDO-serialized all-or-nothing `main` moves, `<p>.composed`).
6. **GitHub export.**

Then the preview work in `newgit-cloudflare.md` continues on top: a preview's
environment identity is simply the set of tracker commits it composes.

---

## Decided with Jack (2026-10-04)

1. **The project root is a workspace:** the instance `main`, bound to every
   tracker's `main`. You work there as today; merges land there.
2. **Secret trackers stay out of the workspace's Git; code trackers are
   tracked.** *(Revised the same day.)* The first answer was to track
   everything, on the grounds that every push goes through newgit — but only
   pushes to `origin` do, so an agent pushing anywhere else would carry a
   committed `.env` with it. A tracker marked `secret = true` (env trackers
   by default) is excluded from the workspace's Git as in v1: never in a
   commit, captured by newgit from the files at checkpoint, so no push can
   carry it. Code trackers are tracked and split into their repos on
   capture. Limit, stated plainly: an agent can read anything in its
   workspace, so this prevents accidental leaks, not a malicious agent.
3. **Merges stack by tracker**: choose which trackers' changes to merge;
   a tracker's changes merge only with the changes of the trackers it
   `requires` (see *Checkpoints, merges, and coherence*).
4. **The video leads with spawning anything, anywhere** — an agent's attempt
   on your machine or as a preview, with its whole environment and your
   keys — with trackers shown as how it works.
