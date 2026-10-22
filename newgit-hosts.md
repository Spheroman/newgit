# Hosts: spawning on another machine

Status: idea, not built.

```
newgit host add devbox --mine
newgit spawn auth-refactor --on devbox
newgit run --on devbox -- claude ...
```

A **host** is where an instance runs: this machine, a machine reachable over
SSH (or a tailnet), or a Cloudflare sandbox. Three backends for one concept.
The motivating case: steer eight agents from a laptop while they run on a
desktop, and open their dev servers in the laptop's browser.

## The shape

This is `spawn --from-checkpoint` pointed at a machine you control instead of
at newgit.dev. Nothing new travels; it is all Git.

1. **Ship bytes with Git.** `git push` the source rev and the bound tracker
   refs into the host's store over SSH. Push, not fetch, so the laptop need
   not be reachable.
2. **Spawn there.** `ssh devbox newgit spawn <name> --rev <sha> ...`. The
   host's own `prepare`/`restore` builds installs, ports, and processes.
3. **The binding record lives on the host**, since pids, ports, and installs
   are host-local. The laptop keeps a pointer (`auth-refactor → devbox`);
   `status`, `run`, `checkpoint`, `undo`, and `remove` pass through over SSH.
4. **Come back the same way:** `spawn --from-checkpoint devbox:<inst>/<ckpt>`.

The agent runs on the host. Editing remote files from a local agent
(sshfs, VS Code Remote) is out of scope.

## Network

- **Tailnet:** exported URLs are just `http://devbox:<port>`. No forwarding.
- **Plain SSH:** a port on the host is unreachable without a tunnel. The
  tunnel is a resource (liveness): newgit owns an `ssh -L` per exported port
  and renders the forwarded local port into `APP_URL`.

Build against a tailnet first; forwarding comes second.

## Secrets (do not simulate security)

A host's root can read every secret tracker an instance binds there. So hosts
are declared, and trust is explicit:

- `--mine`: the host is yours; your `user`-audience trackers go there.
- Otherwise spawn withholds `user`-audience secret trackers and names each
  one, the same way `user:<login>` refusals work on the remote.

## Open questions

- Version skew: require a matching newgit on the host at first; auto-push
  the binary later (as VS Code Server does).
- Does the host's store sync with newgit.dev directly, or only through the
  laptop?
- Should `newgit status` list instances on every known host by default?
