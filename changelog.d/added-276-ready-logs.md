- Every `starting` line and every not-up problem for a running process
  names the log of its run, and the not-up one ends with that log's last
  lines; `spawn` prints where the instance's logs are (`logs:`, and `logs`
  in `--json`) ([#276](https://github.com/Spheroman/newgit/issues/276)).

  A dev server that crashed under a wrapper that stayed alive
  (`npm-run-all`) read `running` and still starting, and nothing said
  where to look; an agent found the log only by searching `.newgit/logs/`.
