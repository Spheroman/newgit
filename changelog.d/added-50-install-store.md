- The **install store**: `[identity] produces` names the tree an install
  builds, and the second instance of a given lockfile is cloned from the
  first instead of installing
  ([#50](https://github.com/Spheroman/newgit/issues/50)).

  Every instance already got its own `node_modules`, and instances at
  *different* lockfiles still must. But instances at the *same* lockfile were
  rebuilding a tree byte-identical to one already on the machine — on a real
  Expo monorepo, ~90 s and 9.0 GB, every time. Declaring what the install
  produces makes that a clone:

  ```toml
  [identity]
  paths       = ["package-lock.json", "package.json"]
  produces    = ["node_modules"]
  key_command = "node -v && uname -sm"
  ```

  ```
  resource:  `deps` prepare: ok
             install store: stored as 1264badb0621 (copy-on-write)

  resource:  `deps` prepare: not needed
             install store: filled from 1264badb0621 (copy-on-write)
  ```

  The key is the content of `paths`, `key_command`'s stdout, and the
  definition file itself — the command that builds a tree is as much an input
  as the lockfile it reads, so editing `prepare` rebuilds rather than reusing
  what the old command built.

  `key_command` is the half a lockfile cannot supply: its hash describes what
  was asked for, not what gets built, and install scripts compile against a
  platform and toolchain it never sees. newgit does not guess at that list —
  what a tree depends on beyond its lockfile is ecosystem knowledge the
  definition has and newgit does not.

  Clones are **copy-on-write, never hardlinks**, and that is correctness
  rather than performance. `node_modules` is not read-only after install:
  native builds write into it (693 files of CMake output under
  `react-native-reanimated/android/.cxx/` on the project this came from), so
  under hardlinks one instance's Android build would rewrite every other
  instance's tree — the exact corruption the design exists to prevent,
  arriving by a route hash-keying alone does not cover. Where the filesystem
  cannot clone, newgit takes a full copy and says so: same behaviour
  everywhere, worse performance, and on such a filesystem an entry costs a
  whole tree on disk rather than almost nothing.

  Copy-on-write does not help if the poison is already in the shared content,
  so a tree is scanned before it is published and declined if it names the
  workspace it was built in, with the file named. That resource keeps
  installing per instance; nothing fails. The store never fails a spawn at
  all — every way it can go wrong degrades to the install that would have
  happened anyway, and says which.

  An undo is filled from the store as well — that rebuild is the expensive
  one, a full reinstall in the middle of an operation someone is waiting on.
  The tree being rewound away from is moved aside and discarded only once the
  clone lands, so a copy that fails partway through leaves the instance with
  the stale tree rather than none. `newgit undo --force-recompute` bypasses
  the store and **drops the entry**: that flag means the identity is not to be
  trusted to describe the tree, a cache keyed on the identity is under the
  same suspicion, and this is the way out if an entry is ever wrong.

  Declared `produces` paths join tracker-owned paths in each workspace's
  `.git/info/exclude`. Derived content is not source, and without the rule a
  project with no `node_modules` entry of its own would have its whole install
  swept into source history by the `git add -A` a checkpoint runs.

  `newgit cleanup` prunes entries no live instance's identity keys to, plus
  anything an interrupted publish left half-copied. The tree is rebuildable
  from the inputs that key it, so the worst a wrong eviction costs is an
  install. Reachability is recomputed rather than recorded — an entry is kept
  exactly when a spawn of that instance would have found it — and gating is
  per resource, so one instance that cannot be keyed does not disable the
  sweep for the whole project. `--dry-run` does not run a `key_command` at
  all: a dry run observes, and spawning a user-supplied shell is acting.

  `newgit action <resource>.prepare` always runs the command. The store
  stands in for a *spawn*, never for a command someone asked for by name —
  but it publishes what that command built.
