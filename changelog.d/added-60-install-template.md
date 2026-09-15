- Added an `install` resource template: a generic dependency-install
  starter with no package manager assumed
  ([#60](https://github.com/Spheroman/newgit/issues/60)).

  `pnpm` was the only install starter, so an npm (or uv, or Cargo) project
  had to begin from a file that was wrong line by line: `depends_on`, the
  lockfile name, and the install command all needed changing, and
  instantiating it also created a `pnpm-store` resource the user then had
  to delete in a specific order. What survived unedited was exactly the
  three things a user could not have guessed — `ownership`, `[checkpoint]
  mode = "hash"`, `[restore] mode = "recompute"` — which was the whole
  reason to start from a template at all.

  Rather than one more named template per package manager (`npm`, `uv`,
  `cargo`, ...), `install` parameterizes nothing: no `depends_on`, no
  companion resource, and its two variable lines — `[identity] paths` and
  the `prepare` command — are `EDIT ME` placeholders instead of a guess
  that happens to be wrong for whichever manager it wasn't written for.
  `pnpm` remains the worked example, since it is also the one that wires up
  a shared, content-addressed store; the reference's *Installs* section now
  points newcomers at `install` when pnpm isn't their package manager.

  `resource add --template pnpm` also now says that its companion
  `pnpm-store` must be removed *after* the resource that depends on it,
  since `resource remove` enforces the order but previously only explained
  why when it refused.
