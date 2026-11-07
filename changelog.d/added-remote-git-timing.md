- Git requests to a remote say where their time went, in a `Server-Timing`
  header: `GIT_TRACE_CURL=1 git ls-remote origin` shows the time in each stage
  and how many Artifacts calls the request made. Durations and counts only.
