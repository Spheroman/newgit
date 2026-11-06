- The `supabase` template's checkpoint and restore pass its own
  `checkpoint --verify`
  ([#241](https://github.com/Spheroman/newgit/issues/241)).

  The checkpoint swaps pg_dump's per-run random `\restrict` key for a fixed
  one, but matched only a line that starts with the backslash, and since
  CLI 2.41.2 `supabase db dump` writes the pair as SQL comments:
  `-- \restrict <key>`. The key stayed random, no two checkpoints of one
  database were the same bytes, and every verify failed. The match now
  takes the line either way.

  The restore resets the database, which replays the migrations and the
  seed and creates the buckets `config.toml` declares, and then emptied
  only `public` before loading the dump. The dump carries `auth` and
  `storage` too, so a bucket a migration makes or a user the seed makes
  was already there when the load reached the dump's copy of it, and the
  INSERT for that table failed on the duplicate key. The CLI writes all of
  a table's rows in one INSERT, so every bucket or user made since went
  too, and psql carried on, so the undo reported success. The restore now
  empties `auth` and `storage` as well, except the migration history the
  dump never carries (`auth.schema_migrations`, `storage.migrations`). It
  no longer restarts sequences, which the load sets anyway and which
  `postgres` does not own in `auth`, and it stops before the load if a
  truncate fails. Existing resource files copied from the template are not
  changed — make the same two edits by hand.
