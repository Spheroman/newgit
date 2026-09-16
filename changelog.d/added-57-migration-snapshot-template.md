- Added a `command-snapshot-migrations` resource template, and named the
  assumption `command-snapshot` was quietly making
  ([#57](https://github.com/Spheroman/newgit/issues/57)).

  `command-snapshot`'s restore is `dropdb`/`createdb` then load, which only
  works when the dump is the whole database — a full dump loaded into a
  target with nothing in it to collide with. That shape is unavailable the
  moment the schema comes from migrations instead (Rails, Prisma,
  Supabase): the database can't be dropped, because recreating it means
  replaying every migration, so the restore becomes reset, then load data
  over rows the migrations already inserted. The dump has to be
  `--data-only`, and the target has to be emptied first — but only of what
  the role running the restore can actually truncate, since the app role is
  rarely the superuser and a table the dump could not read is not one the
  restore has any business emptying either.

  These are two restore strategies, not two settings on one command, so
  they're two templates: `command-snapshot` now says what it assumes in a
  comment and points at the new one, and `command-snapshot-migrations`
  ships the reset-empty-load shape, privilege filter included. Both deposit
  into the same `db-snapshots` lane.

  `definitions.md`'s `[restore]` section also gained one sentence: a
  restore command runs against whatever state its own reset left behind,
  including rows a migration step already inserted, so it is not a fresh
  database unless the command made one.
