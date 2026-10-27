# newgit

**Every branch, ready to run.**

Git tracks your code. Everything else a branch needs to actually *run* lives
outside it: the `.env.local`, the installed dependencies, the dev server, the
database, the ports. All of that is shared by every branch, and you lose it
whenever you switch. That's fine with one branch at a time. With five agents
working in parallel, it breaks.

newgit turns a branch into an **instance**. Each one gets its source, the
files Git shouldn't hold, and the things that have to be running, and all of
it is spawned, checkpointed, undone, and shared as one unit.

```sh
newgit spawn auth-refactor                        # own workspace, env, deps, dev server, ports
newgit checkpoint auth-refactor -m "before agent"
# the agent goes wild
newgit undo auth-refactor                         # source, env files, and snapshots, all back
```

## "Wait, isn't this just `git worktree`?"

A worktree gives you a second copy of your code. Then you copy `.env` over
by hand, run `pnpm install` again, find out port 3000 is taken, and realize
both branches are writing to the same database. newgit is what you'd build
after the tenth time:

- **Trackers** are versioned lanes for files that never belonged in Git:
  env files, local config, an agent's notes. Each has its own history and
  its own audience. They're secret by default, so they're captured and
  checkpointed but never committed.
- **Resources** are the things each branch has to rebuild: installs,
  processes, databases, external services. You declare them once in
  `.newgit/`. They're prepared on spawn and torn down on remove, and every
  instance gets its own ports.
- **Checkpoints** snapshot the whole instance, not just its commits. `undo`
  restores the source, the trackers, and the resources together.

## Principles

1. **Your repo stays yours.** Nothing changes until you ask, and no command
   rewrites history. `git push`, `gh pr create`, and plain `git clone` keep
   working, and GitHub stays where it is.
2. **Secret by default.** Trackers are never committed, and `export` fails
   closed: anything not marked public is withheld and reported.
3. **Never break an undo.** `cleanup` reclaims disk without deleting anything
   a checkpoint still points at.
4. **No special lanes.** newgit doesn't have built-in handling for npm,
   Docker, or Supabase. A resource is a template over commands you already
   run, and a tracker is whatever files you say it is.

## Get started

```sh
cd your-project                                   # any Git repo with a commit
newgit init
newgit tracker create runtime-env --audience user
newgit tracker track runtime-env .env.local
newgit tracker capture runtime-env --from-store   # seed it from the file you already have
newgit resource add deps --template pnpm
newgit resource add app --template process

newgit spawn auth-refactor
newgit action app.start auth-refactor
```

When you like the result, run a plain `git push` from the workspace. It
checkpoints the instance and publishes to your real remote.

## Bring the whole branch with you

With a newgit remote, a checkpoint travels too. When a teammate's branch, or
a cloud agent's, comes up on your machine, it arrives with its uncommitted
work, your own copy of its trackers, and its resources restored. Different
people can read different parts of the same project.

```sh
newgit login
newgit remote add
newgit spawn --from-checkpoint auth-refactor/ckpt_003
newgit merge auth-refactor
```

The hosted remote at [newgit.dev](https://newgit.dev) is invite-only for now.
The Worker in [`remote/`](https://github.com/Spheroman/newgit/tree/main/remote) lets you deploy your own.

## Built for T3 Code

[T3 Code](https://github.com/pingdotgg/t3code) gives every agent thread its
own Git worktree. The [`Spheroman/t3code`](https://github.com/Spheroman/t3code)
fork gives each thread a newgit instance instead, with its env in place, its
dependencies installed, and its dev server on its own port.

The instance follows the thread. You don't delete threads in T3 Code, you
settle them, so that's where the resources go:

- **Settle** a thread and its dev server and containers shut down. The
  workspace stays, with its trackers and checkpoints.
- **Unsettle** it, or send it a message, and they come back up before the
  agent starts.
- **Delete** it and the workspace goes too.

Settle and unsettle are still being wired up
([Spheroman/t3code#2](https://github.com/Spheroman/t3code/issues/2)). Today,
resources are released only when you delete the thread.

```sh
git clone https://github.com/Spheroman/t3code && cd t3code
scripts/fork/install-mac.sh
```

## Install

Prebuilt binaries for macOS and Linux (arm64 and x86_64) are attached to each
[release](https://github.com/Spheroman/newgit/releases):

```sh
TARGET=aarch64-apple-darwin   # or x86_64-apple-darwin, {x86_64,aarch64}-unknown-linux-gnu
VERSION=v0.6.0
BASE=https://github.com/Spheroman/newgit/releases/download/$VERSION
curl -fsSLO $BASE/newgit-$TARGET.tar.gz -O $BASE/newgit-$TARGET.tar.gz.sha256
shasum -a 256 -c newgit-$TARGET.tar.gz.sha256
tar -xzf newgit-$TARGET.tar.gz
install -m755 newgit-$TARGET/newgit ~/.local/bin/newgit
```

Or from source: `cargo install newgit --locked`.

You'll need `git` on your `PATH` and a Unix-like OS. Windows isn't supported.
newgit is `0.x` and moves fast. `newgit --version` prints the commit you're
running.

## Learn more

- **[The guide](https://github.com/Spheroman/newgit/blob/main/docs/guide.md)**
  covers adopting newgit in an existing project, every command, the T3 Code
  integration in detail, exporting to GitHub, and the sharp edges worth
  knowing about.
- **`newgit reference`** prints every tracker and resource key, its default,
  and which template variables each hook sees.
- **The design docs**,
  [`newgit-v1-mvp.md`](https://github.com/Spheroman/newgit/blob/main/newgit-v1-mvp.md)
  and
  [`newgit-architecture.md`](https://github.com/Spheroman/newgit/blob/main/newgit-architecture.md),
  explain why it's shaped this way.

## License

MIT or Apache-2.0, at your option.
