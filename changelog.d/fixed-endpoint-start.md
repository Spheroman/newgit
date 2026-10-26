- The Git endpoint at `https://newgit.dev/<owner>/<name>.git` answers again.
  Since the endpoint learned to sleep, its object set the container's
  inactivity backstop in its constructor, before any container had started;
  that throws, so every clone and fetch failed with a 500. The backstop is
  now set once the container is running.
