- `status` is several times faster in a project with many instances.

  It ran every instance's `git` queries one after another, and only then
  asked the remote what it has not written down yet. With about 130
  instances that came to 3–8 seconds, and the table waited on all of it.
  Now each instance's queries run in parallel, the remote is asked while
  that happens, and the remote's `/state` loads tracker definitions once
  per request instead of once per tracker. The tracker-path gitignore
  check that runs before most commands is also one `git` call per check
  instead of one per path.
