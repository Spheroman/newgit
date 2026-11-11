- `newgit serve` keeps one Git store per audience, each tracker a namespace
  in it with refs in reftable, instead of a repository per tracker
  ([#409](https://github.com/Spheroman/newgit/issues/409)).

  A repository per tracker does not survive a large project: nixpkgs as
  22k trackers was 424k files, a 57 s tree-wide change, and a 143 s gc,
  against 19 files, 7 s, and 4–15 s pooled. Pooling across audiences is
  never safe, since Git serves any object in a repository by id to anyone
  who can reach it, so a store is made for one audience and never takes a
  second; a private instance and each user's content keep stores of their
  own, and a new tracker's seed stays alone until it joins a branch. An
  existing data directory is moved over the first time the server starts
  on it. The server now needs Git 2.45 or newer.
