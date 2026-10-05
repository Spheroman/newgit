- `newgit tracker create <name> --code --path <path>` creates a code
  tracker: committed content with its own history, carved out of whichever
  tracker owned its paths (usually root) — every commit that touched them,
  marked with its source, so nothing is rewritten and composing still
  reproduces the project's history exactly. `newgit init` also writes the
  `project` tracker, which carries the committed parts of `.newgit/`.
  Capturing or tracking paths on a code tracker is refused: you commit to it
  with Git.
