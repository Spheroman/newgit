- `newgit serve` collects its own garbage: the landing log, every store,
  and merges' landings nothing waits in, every six hours
  ([#434](https://github.com/Spheroman/newgit/issues/434)).

  A voided move's copied history and a narrowing's old history used to
  stay in the wider store for good, fetchable by id under protocol v2;
  they now go at the first pass after an hour. A pass holds each home's
  root (a landing there meanwhile is answered 503 naming it) and the log's
  leader's seat, keeps whatever any ref or waiting merge still reaches,
  and never drops a landing-log entry. Stores no longer collect on their
  own (`gc.auto = 0`, `receive.autogc = false`), since a push's
  `gc --auto` knew nothing of what merges borrow. The last pass is in
  `GET /v1/serve/status`; `NEWGIT_SERVE_GC_EVERY_SECONDS` and
  `NEWGIT_SERVE_GC_GRACE_SECONDS` tune it.
