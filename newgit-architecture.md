# newgit — Architecture

*A git-compatible orchestration layer for agentic development workflows.*

---

## Thesis

newgit is **not** a replacement for git's object model. It is an orchestrator that wraps git, reusing twenty years of hardened plumbing (pack files, gc, corruption recovery, concurrent-access safety) instead of re-encountering it as bugs. Almost every pain point that motivated this project — worktree friction, port collisions, redundant installs, secret scoping, network bleed, lossy undo — lives in the *workspace, environment, secrets, and network layers* that git was never designed to touch. The contribution is unifying those layers behind one coherent, git-adjacent UX.

The target customer is **agentic workflows**: many agents spinning up many branch-variants of the same project in parallel, unattended. That customer is what makes the integration *the product* — no human is around to hand-wire `direnv → docker compose → pnpm → worktree` ten times — and it is the lens that resolves nearly every design tradeoff below.

### Non-negotiable constraints

- **Never reimplement git's object model.** Reuse `gitoxide`/`libgit2` as the content-addressed object store; reuse `jj`'s model for the source tracker. Vendor and extend; do not rebuild.
- **Stay format-compatible.** Compete on *experience*, not on *format*. A new on-disk or wire format is what triggers the competing-standards trap. Be a frontend, like `jj` is.
- **The agent authors the policy; it never *is* the policy.** (Elaborated under Projection.)
- **Convenience tools must not wear security tools' clothes.** The encrypted blob is explicitly a convenience substrate and is not recommended for security-minded projects.

---

## Layer map

```
            ┌─────────────────────────────────────────────┐
 Humans  →  │  FUSE / overlay projection  (POSIX, lazy)   │
 Agents  →  │  content API projection     (no kernel)     │
            └───────────────────┬─────────────────────────┘
                                │ projections of ↓
            ┌───────────────────┴─────────────────────────┐
            │   UNIFIED LOCAL OBJECT STORE                 │
            │   one content-addressed graph (gitoxide)     │
            │   partitioned into TRACKERS                  │
            └───────────────────┬─────────────────────────┘
                                │ project(store, policy) at push
        ┌───────────────────────┼───────────────────────┐
        ▼                       ▼                       ▼
  native remote          private GitHub repo      encrypted blob
  (mediated)             (rented ACL)             (convenience)
        ▲                                                 
        └────────── embargo: a TIME policy over any substrate
```

---

## The store

One unified local object store: a single content-addressed graph, git-compatible via `gitoxide`/`libgit2`. This is the OS-independent core — blobs and refs can live in memory, SQLite, a KV store, or S3; nothing here requires a kernel or a POSIX filesystem.

The decision to keep **one** store (rather than one repo per tracker) is load-bearing:

- **Atomicity.** A single logical change spanning several trackers is one commit in one graph, not N uncoordinated commits across N repos. This avoids the silent-revert race that plagues cross-repo sync.
- **Intra-file privacy.** Because the privacy unit is the *object/hunk*, not the repo, a private security fix to a shared file (`auth.ts` belongs to `main`, the fix is a private hunk inside it) is expressible. Repo-per-tracker collapses privacy back to path granularity, which is insufficient.
- **Cheap reclassification.** Promoting/demoting a file between trackers is a frequent privacy operation; within one graph it is metadata, not destructive cross-repo history surgery.

Repos are an **output** of the store, emitted at push — not the storage unit.

---

## Trackers and resources

The store is partitioned into **trackers**; branch instances are animated by **resources**. The split is load-bearing:

> **Trackers hold state that can travel across space and time** — synced to a remote, restored from a checkpoint. **Resources re-establish the state that can't make that trip** — because it is alive, lives in another system, or is only valid where it was built.

### Trackers

A tracker is a named, versioned lane of file content — a partition over the single store: by path where privacy/concern separates cleanly, by object/hunk membership where it cuts through shared files. The user model is Git tracking with finer lanes: a tracker owns paths, capture records current workspace content into that lane, merge promotes a branch's tracker state to the lane head, pull/checkout projects captured lane content back out. It is not a materialization recipe or an environment export mechanism. There is no fixed set of trackers; users define as many as they need (`jack-env`, `db-snapshots`, `design-assets`, …). Each tracker carries three settings:

- **audience** — who may read it (public, project-devs, a single user). This is the unit of the privacy model.
- **merge_with_source** — whether a real source merge should carry this tracker's bound state with it. Env files usually do not; generated code, feature assets, and sub-repo snapshots often do.
- **storage** — where synced state lives: local, native remote, or a rented substrate.

`source` is simply the default tracker: audience = everyone, mechanism = the vendored `jj` engine. The `jj` engine covers exactly **one** tracker (source); every other tracker's source-merge behavior is policy you define in the binding layer.

### Resources

A resource is a lifecycle unit bound to a branch instance. It has no history and never syncs; it consumes tracker content and exports runtime values (ports, URLs, env vars). Resources exist for exactly three irreducible reasons:

- **Liveness.** A running process cannot be copied, only started; a port cannot be snapshotted, only freshly allocated per instance; a daemon's state can only be captured consistently through the daemon.
- **Externality.** Cloud preview environments, webhook tunnels, mock auth tenants — the state lives in another system, the local filesystem holds at most a handle, and an API call is the only interface.
- **Path-dependence.** Installed artifacts (venvs, `node_modules`, native builds) hardcode machine and path; the true state is the identity (the lockfile, already in source) and the artifact is *recomputed* from it, never copied across identities. Recompute is correctness, not optimization — and it keeps derived gigabytes out of the content store. Two instances at the *same* identity are not two derivations: their tree is cloned copy-on-write from the install store, which is keyed on exactly that identity.

The two primitives cooperate at exactly one seam: **a resource's checkpoint hook may deposit its output into a tracker** (`pg_dump` → a `db-snapshots` tracker), turning daemon-owned state into carryable, versioned content.

Under this split the four original lanes decompose: source and env are trackers; install is a resource (path-dependence); db is a resource whose checkpoints land in a tracker.

---

## The binding layer

The genuinely novel part — nobody has built it. It defines how a source revision references *which* revision of every other tracker and *which* concrete instance of every resource (which env content, which lockfile identity, which db snapshot), so that materializing a branch yields a **coherent** set rather than a pile of independently-floating tracks.

The recurring tension this layer must resolve: source, deps, secrets, and db state are *coupled* (the lockfile pins deps to code; migration state pins schema to code), yet à-la-carte tracker composition treats them as independent dials. Systems that deliver reproducibility (Nix) do so by *removing* that freedom — the lockfile is the source of truth and the environment is derived. The binding layer must either pick a side or build guardrails.

One guardrail is that resource dependency state should tell the truth: if a
dependency fails to prepare, dependents are blocked rather than prepared or
started as if the branch were coherent. The branch instance can still exist for
inspection and repair; what must not happen is a downstream `ready` status or
running process built on a failed prerequisite.

> **Open decision.** Does a source revision *pin* exact versions of the other trackers (reproducible, rigid) or *name them loosely* and resolve at materialization (flexible, drifty)? Possibly per-tracker. This is unresolved and sizes the coherence guarantee.

---

## Working copy as projection

The filesystem stops being authoritative. Tracker state is the source of truth; the filesystem is **one projection** of it. Two projections ride on the same store:

- **FUSE / overlay** — POSIX, for humans and legacy tools (compilers, bundlers, LSPs all assume a real filesystem; this never goes away).
- **Content API** — `getFile(path, rev)` / `putFile(path, content) → rev`, for agents and sandboxes with no kernel (e.g. an in-memory bash sandbox).

Both are **lazy**: nothing materializes until touched. This single mechanism dissolves the original complaints — 5-minute installs and full-tree worktree copies — because spinning up branch number ten *mounts a view* rather than copying a tree. "Source control without a real OS" and "instant branch spinup" turn out to be the same problem solved by lazy projection off a content-addressed core.

Honest scope: the filesystem cannot be *eliminated* (every build tool needs POSIX), only made *non-authoritative*. The FUSE/API projection also carries overhead versus raw fs ops, which is why laziness is mandatory rather than optional.

---

## The command surface as projection

The same mediation thesis, applied to the last surface agents touch directly. Filesystem, network, and builds are all intercepted; there is no principled reason `git commit` and `pnpm dev` should be the exception. Because newgit owns the workspace (`spawn` creates it, `run` sets its env), prepending a shim directory to `PATH` intercepts the command surface in any agent harness, with zero agent cooperation.

The design principle is **interpose, don't emulate**. There are two ways to capture a command, and only one of them survives contact with an agent:

- **Passthrough with side effects (correct).** `git commit` runs the real git/jj commit *and* checkpoints the trackers and resources alongside it. `pnpm dev` really starts the dev server — but through the resource definition, with the branch's port injected before the process binds. Nothing the agent observes afterward is false; the command was enriched, not counterfeited.
- **Emulation (never).** A shim that secretly does something *other* than what the command means must then fake every read path (`git log`, `git status`, `.git` itself) well enough that a suspicious agent never notices. Agents are the worst audience for this: when something looks 95% right, an agent doesn't shrug — it *debugs*, and the moment two answers disagree it concludes the repo is corrupt and starts "fixing" it. A leaky lie is strictly worse than no shim. Every claim the environment makes must be verifiable by the tools in that environment.

Shims are not secret; they **announce themselves in command output** (`[newgit] checkpoint ckpt_018: also captured jack-env, supabase → db-snapshots`). Agents read command output more reliably than any documentation, so each intercepted command is a teaching moment — the shim is simultaneously the compatibility layer and the onboarding. The required agent briefing for a newgit project rounds to zero lines, the strongest reading of "obvious = what agents assume by default."

The interception taxonomy:

| Mode | Example | Rule |
|------|---------|------|
| **Enrich** | `git commit`, `npm install` | run the real command, add newgit side effects |
| **Inject** | `pnpm dev` | run the real command through the resource, with ports/env injected |
| **Advise** | `git checkout -b` | let it happen, print the better newgit move |
| **Emulate** | — | never |

`Advise` exists because a few commands *semantically* diverge: `git checkout -b` expects a new branch in *this* directory, while `newgit spawn` creates a workspace elsewhere. Silently redirecting would violate the agent's model of where it is standing. Intercept effects freely; redirect semantics never.

---

## Build-in-store

Builds run against the store in a **hermetic sandbox** (Bazel RBE / Nix model); inputs are a Merkle tree of content digests, outputs land as digests, paths surface into the user's view. Capture is an overlay upper-dir or a FUSE layer recording reads/writes.

The easy part is capture. The whole game is **hermeticity**: the output is only coherent if the true input set is fully declared and the sandbox faults on undeclared reads. The payoff justifies the rigor — once inputs are a content hash, an identical input tree is a guaranteed cache hit, giving a content-addressed build cache for free, which is what makes ten agents building ten variants cheap.

> **Open decision.** Enforce hermeticity (Nix-hard, reproducible, costly against impure npm toolchains) or do best-effort overlay capture (works most of the time, silently non-reproducible on impure builds). The JS ecosystem defaults to the impure side; choose knowingly.

---

## Per-branch network isolation *(open question)*

The original goal: each branch gets its own named network (`feature-x.app.localhost`, or a `.<project>` namespace), branches are **blind to each other**, port collisions disappear, and "everything runs through a proxy." The added constraint: **no containers** — assemble the kernel primitives directly rather than pull in Docker.

The reframe that makes "without Docker" trivial: Docker's network isolation is not magic — it is **network namespaces + veth + bridge + nftables + a DNS resolver**. newgit can use the *same kernel primitives without the container*, i.e. without the cgroup/mount/pid isolation that makes something "a container." So dropping Docker isn't a limitation; it's using a narrower subset of the same machinery.

The real open question is *where on the isolation spectrum to land:*

- **Tier 0 — loopback aliasing (no isolation).** Bind branch A to `127.0.0.2`, branch B to `127.0.0.3`, … The whole `127.0.0.0/8` is loopback on Linux, so each branch gets the *full* port range on its own IP; a resolver maps `feature-a.app.localhost → 127.0.0.2`. Solves collision + naming, rootless, trivial. Does **not** provide blindness — every branch can still reach every other branch's loopback IP. (macOS needs an explicit `lo0` alias per IP; less clean.)
- **Tier 1 — network namespaces (real blindness).** One netns per branch. Each namespace has its own loopback and port space, so `:3000` in A is physically separate from `:3000` in B and **inter-branch blindness is by construction** — a namespace cannot see another's services without explicit plumbing. The work is egress and host-reachability: a veth pair + bridge (needs root) or **rootless userspace networking via `pasta`/`slirp4netns`** (no root). A per-namespace `/etc/netns/<branch>/resolv.conf` scopes *naming* too, so a branch resolves only its own services.

**The egress nuance** (the part "blind to each other" gets wrong): total blindness is incorrect — branches need shared *outbound* (the real internet, a shared auth provider, sometimes one shared real DB). The true spec is **isolated from each other, controlled egress outward**, expressed as nftables policy (deny inter-namespace, allow egress) or `pasta`'s outbound-only default.

**The proxy, reframed.** "Run everything through a proxy" is the same mediation thesis as the rest of newgit — network becomes another *intercepted/projected* resource, per branch. A host reverse proxy routes inbound by `Host` header to each namespace; outbound is redirected (via nftables) to a per-branch egress proxy. That lets newgit give each branch its own network identity, observe and control what it reaches, and optionally route API calls to branch-specific backends — the network analogue of capturing filesystem writes.

**Why this matters for the agentic customer (beyond ergonomics):** per-branch isolation is *blast-radius reduction*. An agent in branch A should not be able to reach branch B's database or exfiltrate across branches; namespace blindness makes that structural, the same way the sandbox bounds filesystem damage. So Tier 1 is a security boundary, not just a convenience.

> **Open question.** Tier 0 (loopback aliasing — trivial, rootless, no isolation) or Tier 1 (network namespaces — real blindness, rootless via `pasta`, more plumbing)? The deciding axis is the same as everywhere else: mutually-trusting variants of one developer's work → Tier 0 suffices; branches running untrusted agents that must not reach each other → Tier 1's blindness is a security primitive. Naming via a `.localhost`/`.test` wildcard (dnsmasq or systemd-resolved); avoid `.local` (mDNS collision); an arbitrary `.<project>` TLD requires an explicit resolver entry.

---

## History capture

`jj` already auto-snapshots the working copy on every *command* and exposes an operation log ("undo operations one by one"). newgit increases capture *frequency* to every **write** — a filesystem watcher / FUSE layer feeding snapshots into the same backend — so the "agent wrote a change, I hate it, undo N steps" case is a linear revert rather than re-prompting the model to walk it back. Snapshots squash into clean commits at checkpoint boundaries.

This is orthogonal to the object model: snapshot more often into the same store, squash later. Content-addressing keeps it cheap for text (unchanged file = same blob hash). Capture rules are **per-tracker**: source = every-write; a `db-snapshots` tracker fills only via a resource's checkpoint hook (snapshotting a large binary on every write defeats dedup). Keep the write-firehose off snapshot-fed trackers.

Linear revert covers the stated use case. *Selective* fine-grained undo (undo edit-40-of-80 in isolation) would need a CRDT/op-model layered on top — explicitly **not** built, because the use case doesn't require it.

---

## The projection engine — `project(private, policy)`

The spine of the privacy model and the answer to "how is public derived from private." Privacy is **private-by-default**; public-facing trackers are explicitly created and **promoted** into. The public history is a constructed artifact that does not exist locally — it is pure output, assembled at push — which is *why* it leaks no structure of the private history.

`project` is a **history** projection (per private revision → zero or more public revisions), not a state projection, so the correspondence survives. `policy` has three layers in increasing difficulty:

1. **Inclusion / exclusion** — which trackers and paths project at all. Pure globs. This is where `.env` and proprietary modules simply never match.
2. **Transformations** — path moves, string/AST replacements, stripping of marked internal-only regions. Pure functions on text/AST.
3. **Coherence shims** — dependency-closure repairs (the public fix references a symbol a private rename removed; rewrite it to apply against public context). Authored by the agent, then **pinned into the policy and replayed** — never regenerated per push.

Requirements:

- The policy language is **pure and total** (Starlark-class) so two actors projecting the same private state get byte-identical public output.
- A **machine-checkable correspondence**: every public revision back-references the `(private rev, policy rev)` it came from (generalizing Copybara's destination-stored labels). Anyone can re-run `project` and verify the public repo *is* exactly `f(private, policy)`.

**Why deterministic and not agent-as-projection:** you audit `f` once (small, stable, reviewable) instead of re-reviewing model output on every push. Determinism buys **auditability**; per-push model output destroys it and resurrects the manual labor at the worst point.

### The agent's role

The agent **authors and repairs the policy** (cheap, parallel — exactly the economics that make this viable) and is the right tool for **coherence** (transplant-repair, which CI catches when wrong). It is the *wrong* tool for **concealment**, because concealment is not CI-checkable and the agent's wrongness there is undetectable. The agent orbits `project`; it never *is* `project`.

### Failure modes designed-around

- **Shim rot.** A shim authored against private-state-T1 stops applying when private drifts to T2. `project` is deterministic *given a pair*, not stable as private moves. CI on the projected tree is the rot detector → agent proposes a *policy* patch → small policy diff reviewed. The agent maintains `f`; it never becomes `f`.
- **Bidirectional flow.** Inbound contributions (the whole point of "more projects could be open source") are public→private transplants and must be supported, not just private→public. Production teams gate this with a **sync check run immediately before merge** (merge-queue style) to prevent the silent-revert race; newgit needs the same as a primitive.

---

## Enforcement substrates

Every option is the *same* projection engine with a different **enforcement substrate**, sorted by *who you are hiding from*. The user picks per adversary.

| Substrate | Mediation | Hides content | Hides metadata | Hides **shape** | Cost / trust |
|-----------|-----------|:---:|:---:|:---:|--------------|
| **Native newgit remote** | server-mediated retrieval | ✅ | ✅ | ✅ | you run it; strongest; *young* |
| **Private GitHub repo per tracker** | rented (GitHub ACL) | ✅ | ✅ | ❌ | free; path-separable only; trust GitHub |
| **Encrypted blob in private repo** | none (crypto only) | ✅ | ❌ | ❌ | convenience; leaks shape by construction |

**Positioning (decided):** shape-concealment *is* a real promise. Therefore the **native remote and embargo are core**; GitHub and blob are the explicitly weaker, convenient tier.

- The **encrypted blob is convenience only** and is **not recommended for security-minded projects**. It is correct for content-privacy among the mostly-trusted, and wrong for the embargoed-fix case (where the adversary may have private-repo read access and *shape is the secret*).
- For real security: **stand up a newgit remote and use the native implementation.**

### Native remote — implementation

The native remote is a **dumb, auth-gated store of client-precomputed projection bundles** — *not* a serve-time projection engine.

Because `project` is already a deterministic client-side function, the authoring client computes one bundle per audience tier (`project(full_store, tier_policy)`) and uploads them. The server only does auth-gated retrieval. Consequences:

- **Bounded breach radius.** The server never holds cross-tier plaintext it could leak; a breach exposes only the most-privileged bundle stored there, not the entire truth.
- **Shape concealment for free.** Each bundle is a coherent standalone history — stripping happened before upload — so there is no hole, no silhouette, nothing referenced-but-absent.
- **Efficient dedup.** Content-address-share objects common across tiers (sharing a common object is not a leak); duplicate only tier-exclusive objects (whose mere presence in a lower bundle *would* be the leak).

This keeps the one from-scratch, security-critical component **simple enough to actually harden**, and reuses the projection machinery rather than building a second server-side copy of it.

---

## Embargo — a time policy, not a substrate

The silhouette attack is traffic analysis: a sustained public history with a persistent hole lets an observer recover the shape of the hidden work from the negative space everything routes around. Determinism cannot fix this (it provably hides *what you told it to*, and that exclusion is itself visible). Two real mitigations exist, both about the observation window:

- **Embargo (strong).** Develop the shape-sensitive change off *every* published surface and publish **atomically** at disclosure. No persistent hole ⇒ no silhouette. This is coordinated-disclosure practice and it rides on *any* substrate.
- **Cover traffic (weak).** Decoy churn around the region; raises attacker cost, never a proof, expensive. A differential-privacy posture.

Embargo is orthogonal to content concealment and a complete system needs both axes, because they fail in different directions.

---

## CI and the tripwire

- **CI is the coherence oracle.** The projected tree failing to build is the detector for shim rot and incoherent promotions.
- **The tripwire makes residual shape-leak perceptible.** Treat redacted private symbols as taint; flag when public changes start *clustering structurally* around the redaction boundary (when an edit's only reason for its shape is routing around a private hole). It is a heuristic, not a proof — but a `.env` leak is visible/immediate/attributable and a silhouette leak is none of those, so "trust the user" is only honest if the tool makes the invisible risk **perceptible**.

---

## How this answers Theo's four points

1. **Partial open source / privacy granularity** → private-by-default store + projection + per-substrate enforcement; intra-file privacy via object/hunk-level trackers.
2. **Commits/branches are a bad primitive** → reuse `jj` (working-copy-as-commit, anonymous branches, operation log) for the source tracker.
3. **Worktrees are an abomination** → CoW/overlay + lazy projection; "check out the same branch in two places" and "no manual worktree updates" fall out of projecting rather than copying.
4. **Source control shouldn't require a real OS + filesystem** → the OS-independent object store as core, with FUSE *and* content-API projections; the filesystem is non-authoritative and lazy.

---

## The trust root and residual problems (honest)

Routing "real security" to the native remote **concentrates the entire security promise onto the youngest, least-proven, from-scratch component** — and it now faces an *active adversary*, a categorically harder bar than the careless-user assumption the rest of the system degrades against. The precomputed-bundle design minimizes its trust surface, but the honest framing to a security-minded user is:

> newgit's native remote gives shape-concealment GitHub structurally cannot, via projected bundles with a minimal trust surface — **and** it is young infrastructure that earns trust over years, not at v1. Treat it on day one like any new secrets-bearing system, not a proven fortress. For the first while, GitHub's fifteen-years-attacked ACL may be the more trustworthy substrate for content-only privacy.

Irreducible residuals (no architecture removes these):

- **The truth-holder.** Someone holds the most-privileged bundle and its key. Privacy bottoms out in "one party knows everything." That is the floor.
- **Non-interference is undecidable in general.** The silhouette can be mitigated (embargo) and made perceptible (tripwire) but not provably eliminated.
- **Shim rot is perpetual maintenance** (agent + CI), not a one-time cost.
- **Hardening the remote is permanent operational discipline**, not a feature you finish shipping. It starts the day the first secret lands on it.

---

## Open decisions still to make

1. **Binding-layer resolution:** rigid pin vs loose resolve, possibly per-tracker. Sizes the coherence guarantee.
2. **Build-in-store hermeticity:** enforced (Nix-hard) vs best-effort overlay capture.
3. **`jj` integration:** fork-and-track upstream now (the backend trait is not yet a stable public API) vs wait for a clean plugin seam.
4. **Workload ratio:** how much of real usage is **coherence** (detangling parallel-agent changes — the common case, possibly ~95%) vs **concealment** (genuinely hiding security work — the sharp minority). This decides whether newgit is a thin federation/projection tool with a security escape hatch, or a security-first projection engine. Most of the heavy machinery (native remote, embargo, tripwire) only earns its weight if concealment is first-class.
5. **Network isolation tier:** loopback aliasing (collision + naming only, no blindness) vs network namespaces (real inter-branch blindness, rootless via `pasta`, more plumbing). See *Per-branch network isolation*; the deciding axis is whether branches run mutually-untrusted agents.

---

## What the design does *not* do

No part of this reimplements git's object model. The components are: a unified store (gitoxide), `jj`'s model for source, trackers as store partitions, resources as per-branch lifecycle units, the binding layer, two working-copy projections, the projection engine, three enforcement substrates, embargo as a time policy, the agent as policy-author, CI as coherence oracle, and the tripwire as perceptibility layer. Each piece has exactly one home; nothing does two jobs. The remaining hard work is not architectural — it is operational discipline around the remote as a trust root.
