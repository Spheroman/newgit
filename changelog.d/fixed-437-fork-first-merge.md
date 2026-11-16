- `newgit tracker merge <fork>` works before the fork's first checkpoint
  on `newgit serve`
  ([#437](https://github.com/Spheroman/newgit/issues/437)). The server
  makes a fork's repo on its first write, starting it at its upstream's
  `main`, while the client pushed expecting no `main` at all and was
  rejected with `(stale info)`. The client now fetches the fork's `main`
  again after that first write, holds the merge to it, and leases on it.
- A tracker the remote serves narrower than its definition says (a fork
  deleted and re-added under its old name is still held to the upstream
  its repo was made from) now says so on `newgit tracker merge`: a write
  credential carries the reason. It failed closed before, without a word.
- On `newgit serve`, the landing that makes a fork carries the epoch its
  start was checked at, so a change to the definitions between the check
  and the landing (its upstream narrowed) refuses the fork instead of
  making it on stale facts.
