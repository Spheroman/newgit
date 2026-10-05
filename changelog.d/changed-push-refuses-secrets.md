- A `git push` from a workspace is refused when any commit it sends — on any
  branch or tag — adds or changes a file a secret tracker owns, naming the
  commit and file. A remote checkpoint sync checks the same, including its
  uncommitted-work commit. History from before newgit (root's `main`) is
  exempt, so an old committed `.env` never blocks a first push.
- Composing decides ownership among code trackers only, so history that once
  committed a now-secret file composes with its commit ids unchanged.
- CLI output says a tracker's `main` where it said "lane head".
