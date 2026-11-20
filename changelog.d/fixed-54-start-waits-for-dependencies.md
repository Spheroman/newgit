- A resource's `autostart` process starts once what it depends on is up,
  not merely once that process exists
  ([#54](https://github.com/Spheroman/newgit/issues/54)).

  `depends_on` ordered the starts, but a dependent was started the moment
  its dependency's process had been exec'd, so `functions serve` raced a
  Supabase stack or API server that was still coming up, and the race
  just moved into the dependent. Now `spawn`, `newgit start`, and `newgit
  action` of a long-running action by hand give each running dependency
  five seconds to answer on every port it claims, and do not start a
  dependent against one whose `autostart` action is not running. A
  dependent withheld this way says which dependency it waited on, and
  stays owed: the next `newgit start` starts it, as it does an `autostart`
  action that died.
