- A merge's checks no longer wait on the remote's container. The remote
  checks the index and where each pin moves itself, reading commits and
  trees straight from Artifacts. Before, it started the Git endpoint's
  container and fetched histories, while `newgit merge` asked again every
  ten seconds. That wait was about 10 of a live merge's 18 seconds. The
  container still answers when a commit can't be read or a walk runs past
  200 commits. When it does, a request now waits up to 20 seconds for the
  answer before saying "still checking".
