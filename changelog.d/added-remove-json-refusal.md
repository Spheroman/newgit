- **`newgit remove --json` says why it refused.** A refusal printed nothing,
  so a caller could not tell a removal `newgit start` would unblock from one
  only `--force` gets past. It now prints `{"instance", "removed": false,
  "refused": {"reason", "message", "resources", "checkpoint", "next"}}` and
  still exits 1; `next` is `newgit start <instance>` when a resource whose
  capture failed has a process that is not running. A removal that goes
  through has `"removed": true`.
