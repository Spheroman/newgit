# The workflow

The whole loop, on one machine:

```sh
newgit init
newgit tracker create runtime-env --audience user
newgit tracker track runtime-env .env.local
newgit tracker capture runtime-env --from-store   # seed the lane from the file you already have
newgit resource add deps --template pnpm
newgit resource add app --template process

newgit spawn auth-refactor                        # installs deps, starts the dev server
newgit checkpoint auth-refactor -m "before agent"
# an agent works in the branch workspace, and you dislike the result
newgit undo auth-refactor
# ...or you like it: from the workspace, a plain `git push` checkpoints and
# publishes to the project's real remote

newgit export auth-refactor --to ../public-export
newgit cleanup
```

With a remote (the hosted one, `https://newgit.dev`, is invite-only for
now; the Worker in `remote/` deploys your own), the same checkpoint
travels: a teammate, or a cloud agent's checkpoint, comes up on your machine
with its source, uncommitted work, your own copy of its trackers, and its
resources restored.

```sh
newgit login                      # GitHub device flow
newgit remote add                 # create <you>/<project>: a repo per tracker in Git
newgit members add octocat
newgit spawn --from-checkpoint auth-refactor/ckpt_003
newgit pr create auth-refactor    # or propose it, and a reviewer runs it by number:
newgit pr checkout 12             # code, database, and ports, as instance pr-12
newgit merge auth-refactor        # into main: one merge commit, every tracker at once

# a teammate
newgit clone jack/widget          # or plain: git clone https://newgit.dev/jack/widget.git
```

Plain `git clone` works too: the remote composes each branch from the trackers in
Git you may read, so two people can read different parts of one
project.
