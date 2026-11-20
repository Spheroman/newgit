- A `long_running` action's stdin is a pipe that stays open for as long as
  it runs, instead of `/dev/null`.

  A watcher that exits when stdin reaches end-of-file (`esbuild --watch`:
  "stopped automatically because stdin was closed") exited at once, while
  the wrapper in front of it (`npm run dev`) stayed alive, so the resource
  read as running and nothing was built or served. The small `sh` newgit
  leaves waiting on the process holds the pipe's write end, as a process
  manager would. One-shot hooks still get no input.
