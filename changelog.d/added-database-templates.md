- Two resource templates for a per-branch database: `postgres` and
  `compose`. Add either as `database` (`newgit resource add database
  --template postgres`) and each instance's `DATABASE_URL` is its own.

  `postgres` gives every instance a whole Postgres cluster without Docker:
  its own supervised server on its own port, data in a gitignored
  `.postgres/` in the workspace, `pg_isready` as its `[ready]` check, a
  `pg_dumpall` of the whole cluster on checkpoint, and a restore that
  builds a fresh cluster beside the live one and swaps it in only once the
  load succeeds. `compose` gives every instance its own Compose project
  (`COMPOSE_PROJECT_NAME`, which `docker compose` under `newgit run` then
  reaches with no `-p`), ports passed in as variables the compose file
  reads, suspend and resume as `stop` and `start`, and `down --volumes` on
  removal. It checkpoints a Postgres service through its own `pg_dump` and
  says plainly that Compose has no generic checkpoint. Before these, the
  only database templates assumed a server on the host that every instance
  shared, or Supabase.
