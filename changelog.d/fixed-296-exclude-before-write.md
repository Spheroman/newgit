- A secret tracker's paths are excluded from a workspace's Git before its
  content is written there
  ([#296](https://github.com/Spheroman/newgit/issues/296)).

  `tracker pull`, `checkout`, and `use` wrote a tracker's files into a
  workspace spawned before that tracker was defined without excluding them,
  so Git listed them as untracked and a plain `git add -A` could commit
  secret content until the next checkpoint added the exclude. Now the one
  function every such write goes through excludes first. New paths also
  join the existing block in `.git/info/exclude` instead of adding a second
  copy of its header.
