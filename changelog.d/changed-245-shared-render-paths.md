- Several resources may now `[[render]]` into the same file, each declaring
  only the values it owns
  ([#245](https://github.com/Spheroman/newgit/issues/245)).

  One file often needs values from several resources: an app's `.env.local`
  wants its own port and the database's URL. With one owner per file, every
  rule had to live on whichever resource got the file, so a `db` definition
  ended up knowing every app's env files. Now the file is one simultaneous
  batch over every resource's rules, located in committed content, so it
  does not depend on which resource bound first; capture, undo, and drift
  work on all of them, and `tracker drop` takes back only the dropped
  resource's values. Two resources claiming the same text are still refused,
  and the error now names both resources and both `find`s and points at
  `{{exports.NAME}}`.
