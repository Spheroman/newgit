# Trackers

Every tracker is its own history. `root` owns `.` — whatever no more specific
tracker claims, `.newgit/` included — and starts as your project's Git
history; `access`, mounted at `.newgit/trackers/`, holds who may read what,
which only the project's owner changes by default (`maintainers = "owner"`).
Carve out a tracker in Git with `newgit tracker create electron --git --path
apps/electron`: one commit that mounts it there, with its history starting
at the carve. Carved inside another mounted tracker, it is that tracker's
child, defined in that tracker's own `.newgit/trackers/`: bound only where its
parent is, and brought along with it unless its own `priority` is negative. So
a large directory can stay out of workspaces while the parent's own repository
still holds it. Trackers are named like files: `t3code/repos` from the top of a
project, `repos` from inside `apps/t3code`, `..` to go up, and a leading `/` for
the full path, such as `/remote/<owner>/<project>/t3code/repos`. Without `--git` a tracker stays outside Git: captured, never
committed.

An instance works on what it binds:

```sh
newgit spawn fix-db --use staging-db --drop electron   # one per role; the rest as you say
newgit tracker drop electron --stash   # out of the working tree, history untouched
newgit tracker use electron            # back, with the stashed work
newgit status                          # ready / starting / not ready, and why
```

Trackers sharing a `role` are alternatives (`dev-db`, `staging-db`); a spawn
picks one by `--use`, then your own default in `~/.newgit/config.toml`, then
`priority`. A tracker can bring resources (they run only while it is bound) and
`require` others.
