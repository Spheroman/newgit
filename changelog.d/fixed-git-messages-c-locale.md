- newgit's retries for racing Git now work under a non-English locale.

  newgit recognises some Git failures by Git's message: losing a race for
  a ref (`not updating`, `cannot lock ref`), or a fetch meeting another
  Git's repack (`shallow file has changed since we read it`). Git
  translates those messages, so with a German or Japanese locale the match
  never fired and the retry never ran. Every Git newgit and `newgit serve`
  run for themselves now go through one helper that sets `LC_ALL=C`; Git
  output meant for you, like `newgit log`, keeps your locale. A test fails
  on any new Git started another way.
