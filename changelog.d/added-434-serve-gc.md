- `newgit serve` collects its own garbage: the landing log, every store,
  and merges' landings nothing waits in, every six hours
  ([#434](https://github.com/Spheroman/newgit/issues/434)).

  A voided move's copied history, a narrowing's old history, and a force
  push's left-behind commits used to stay in their store for good,
  fetchable by id under protocol v2; they now go at the first pass a grace
  (an hour) after nothing reaches them, so within seven hours. Stores keep
  reflogs, but the pass expires their entries after the grace too, so a
  reflog never keeps anything for Git's default 30 days. The first pass
  runs a grace after start, so a server restarted often still collects.
  Each store is collected holding its home's root and compose lock and
  nothing else, so a landing there (answered 503 naming the collection
  after the root wait) or a fetch of a rooted view waits for one store's
  `git gc`, not the whole home's; the log is collected holding its
  leader's seat. Whatever any ref or waiting merge still reaches is kept,
  a landing-log entry is never dropped, and a stored rooted history whose
  views were collected composes them again. Stores no longer collect on
  their own (`gc.auto = 0`, `receive.autogc = false`), since a push's
  `gc --auto` knew nothing of what merges borrow. The last pass is in
  `GET /v1/serve/status`; `NEWGIT_SERVE_GC_EVERY_SECONDS` and
  `NEWGIT_SERVE_GC_GRACE_SECONDS` tune it.
