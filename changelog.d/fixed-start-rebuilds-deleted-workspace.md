- `newgit start` rebuilds a workspace deleted by hand instead of leaving
  `remove` as the only way out.

  With the workspace directory gone, `status` said `ws-missing` and
  nothing else, `start` said there was nothing to do while the old
  processes kept running from the deleted folder, and `spawn` refused
  because the record existed. Workspaces are disposable and the binding
  record is not, so now `status` names `newgit start <name>` (and `status
  --json` lists it under `problems`), and `start` stops what still runs
  from the deleted directory, then rebuilds the workspace from the
  instance's latest checkpoint the way it brings back an evicted one, or,
  with no checkpoint, from its source branch with trackers at their bound
  revs and every resource prepared again on the ports it holds. It says
  what did not come back (whatever was never checkpointed), and `--json`
  reports `rebuilt` with the checkpoint, `null` when there was none.
