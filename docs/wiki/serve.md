# Running your own remote

`newgit serve` is the newgit remote as one process, built into the `newgit`
binary: each tracker's Git history on the machine's own disk, project state in
one SQLite database, and the same API newgit.dev answers. A project points at
it the way it points at newgit.dev, and nothing else about using newgit
changes. It needs `git` 2.46 or newer on the `PATH` (it refuses to start
on an older one), and runs on Linux and macOS.

```sh
newgit serve --data /var/lib/newgit --listen 0.0.0.0:8484 \
  --url https://git.example.com --allow '*' --admin jack \
  --github-client-id <oauth app id>
```

It prints `newgit serve: listening on <url>` and stays in the foreground;
Ctrl-C stops it, and starting it again on the same `--data` carries on where
it was.

- `--data <dir>` is everything the remote keeps: `landing.git`, the log
  every landing is committed to first (if the server stops mid-landing, it
  finishes it, or never shows it, when it starts again), `server.sqlite`,
  the `token.key` tokens are signed with, and under
  `homes/<owner>/<project>/stores/` one Git repository per audience (the
  members', the public's, each user's, each private instance's), every
  tracker a namespace in its audience's, under `landing/` each merge's
  objects until it lands, and under `scratch/` the server's own merges
  while they run. Back it up as a directory, with the
  server stopped. There is no online backup yet. A data directory from
  before stores, with a repository per tracker, is moved over the first time
  the server starts on it, and one from before the landing log gets a log
  that starts from what its database says.
- `--listen` is `127.0.0.1:8484` unless you say otherwise, and `127.0.0.1:0`
  picks a free port and prints it.
- `--url` is the address clients reach it at, when that is not the one they
  asked by. TLS is not the binary's job: put Caddy or nginx in front, and name
  the public address here.
- `--allow` lists the GitHub logins who may log in, or `*` for anyone. It
  starts empty, which admits no one. `--block` shuts logins out whatever
  `--allow` says, `--admin` names who may delete any project and own any
  number, and `--projects-per-login` (5) caps everyone else.
- `--github-client-id` is a GitHub OAuth app's client id, with device flow
  enabled on the app, for `newgit login`. Without it `newgit login` is refused
  with `this remote has no GitHub app configured`.
- `--git-slots` (16) is how many `git` processes may run at once.

## Pointing a project at it

In the project, log in and link it:

```sh
newgit login --url https://git.example.com
newgit remote add --url https://git.example.com
```

`remote add` creates `<you>/<project>` on the server, with each tracker in Git
in its audience's store, and writes `[remote]` to `.newgit/config.toml`. Commit that
file, and the project's other commands (`login`, `merge`, `members`, a
workspace's `git push`) use this remote without being told. The owner adds a
teammate with `newgit members add <login>`, and the teammate then:

```sh
newgit login --url https://git.example.com
newgit clone jack/widget --url https://git.example.com
newgit spawn --from-checkpoint auth-refactor/ckpt_003
```

Plain Git reads it too: `git clone https://git.example.com/jack/widget.git`
gets the project's own commit ids, composed from the trackers you may read.
The Git password is your newgit token, and the server checks it on every
request, so removing a member takes effect on their next fetch.

To try it on one machine, start it with `--dev-login-secret <secret>` (or
`NEWGIT_SERVE_DEV_LOGIN_SECRET`) and no GitHub app. A caller holding the
secret logs in as any login `--allow` admits:

```sh
curl -X POST http://127.0.0.1:8484/v1/dev/login \
  -H 'x-dev-login: <secret>' -d '{"login": "jack"}'
```

The answer holds `token` and `token_id`. `newgit login` cannot use this, since
it is GitHub's device flow, so save them where it would have, in
`~/.newgit/auth.toml`:

```toml
[remotes."http://127.0.0.1:8484"]
login = "jack"
token = "ngt_..."
token_id = "tok_..."
```

Never give a remote anyone can reach a dev login secret.

## Against newgit.dev

The rules are the same: who may read and merge each tracker, members, tokens,
checkpoints, forks, tags, `newgit merge`, `clone`, and `spawn
--from-checkpoint`. Policy is read from each tracker's `main`, as on newgit.dev.
A project moves between them by `remote add --url`, and a CLI that works
against one works against the other.

What differs is the machinery, and it is the reason to run it:

- It is fast. Without Artifacts, a Durable Object, and a container between a
  request and the repository, `remote add` takes about 0.85 s on one machine
  (9.4 s on newgit.dev), a plain `git clone` 0.4 s, a merge 1.2 s, and
  `spawn --from-checkpoint` 1.1 s. Those numbers have no internet round trips
  in them.
- A fork costs nothing. It is a Git namespace inside the tracker's, not a
  copy, except for a private instance's, which gets a store of its own.
- Trackers share storage only with trackers of the same audience. Git lets
  anyone who can reach a repository fetch any object in it by id, so the
  server never puts two audiences' objects in one, rather than pretending to
  filter what Git serves.
- Every landing has a stamp, its place in the landing log, and a branch's
  answer says the stamp of the landing that last moved it.
  `GET /v1/projects/<owner>/<project>/branches/<branch>?at=<stamp>` gives
  the branch's list as that landing left it.
- `newgit merge` lands or it doesn't. When nothing landed since your merge
  began, the branch fast-forwards to your own commits. When something did,
  the server merges your work onto it, as GitHub's merge button does: one
  merge commit in each tracker both of you changed, your commit its second
  parent, and your commits as you made them everywhere else; `newgit merge`
  says so, with the commit. You are told to merge again only for a real
  conflict, and the answer names each file you may read. What lands is
  checked against the rules at the state it lands on, so someone removed
  as a maintainer or member while your merge ran does not land it, and a
  merge that would put your edit in a tracker you may not change (because
  someone moved the file there) is refused, naming the tracker. A merge
  never carries content into a tracker more people may read: if someone
  moved a file into one while you edited it, bring `main` in and make the
  move yours. A merge in flight is kept in memory, so restarting the server
  tells its merger to start the merge over.

- GitHub mirrors, with `--github <file>` (only its owner may read it):

  ```toml
  [projects."jack/store"]
  webhook_secret = "the secret you give the mirror's webhook"
  token = "a token that can read and write the mirror's contents"
  ```

  Each export target on GitHub gets the project's view pushed to it after
  every landing, without what the target withholds, and `newgit status`
  says when one is behind or failing. Give the mirror a webhook at
  `https://<your remote>/v1/webhooks/github?project=<owner>/<name>` with
  that secret and the *push* and *pull request* events, and a pull request
  merged there lands in `main` like anyone's merge, as whoever merged it
  (who must be a member who may merge root). When one is refused,
  `newgit status` says why and what to do.

What it does not do yet:

- Tags made on a GitHub mirror are not brought in.
- The site: no landing page, no `/install.sh`, and no wiki at `/docs`.
- The spend kill switch, backups, and replicas.
- Taking over newgit.dev. That still runs on the Worker, until moving its
  data across is designed and tested.

[`docs/design/serve.md`](https://github.com/Spheroman/newgit/blob/main/docs/design/serve.md)
has the layout, the decisions, and the full list.
