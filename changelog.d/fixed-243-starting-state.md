- A dev server that is still compiling is no longer reported as not ready
  ([#243](https://github.com/Spheroman/newgit/issues/243)).

  `spawn` and `status` used to call a running process whose port had not
  answered yet a problem, so a healthy Next.js spawn looked like a
  failure. That is now its own state, `starting`: it does not count as not
  ready, and the message says the process is still starting and that
  `newgit status` shows when it is up. After the autostart, `spawn` waits up
  to five seconds for the ports to answer and reports only what is still
  unanswered; `spawn --json` lists it under `starting`.
