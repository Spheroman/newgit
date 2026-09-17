- A `supabase` resource template, and every template now states the host
  tools it assumes
  ([#90](https://github.com/Spheroman/newgit/issues/90)).

  `command-snapshot-migrations` named its audience in its own opening
  comment — "Rails, Prisma, **Supabase**, and anything else that runs
  `migrate`" — and then handed out `createdb`, `pg_dump` and `psql`. A
  Supabase user has none of those against their database by construction:
  it lives in a container the CLI manages. Same for a Prisma user on Docker
  Compose, which is most of them. The template read as "ready to adapt"
  when it was really "ready to adapt *if your database is on the host*".

  `newgit resource add db --template supabase` now writes the shape that
  actually works: `supabase start` for `prepare`, `supabase db dump --local
  --data-only` into the `db-snapshots` lane for `checkpoint`, reset →
  truncate-what-this-role-may-truncate → load for `restore`, and `supabase
  stop --no-backup` for `cleanup` — every database command through the CLI
  or `docker exec`, never a host binary. It renders `project_id` and all six
  published ports out of the committed `supabase/config.toml`, and carries a
  commented-out `additional_redirect_urls` render for the Expo case, which
  is a data edge rather than a `depends_on`.

  `project_id` is the load-bearing line: it scopes every container name and
  Docker volume, so without a per-instance value the second instance's
  `supabase start` adopts the first instance's stack. It ships as an
  explicit `EDIT_ME_PROJECT`, so an unedited template refuses at spawn
  naming the file and the string rather than quietly sharing a database
  between branches.

  The smaller half of the same report: all seven templates now open with a
  `HOST TOOLS:` line. The two host-Postgres ones say so and point at
  `supabase` or at putting `docker exec` in front; `install` admits that its
  `key_command` ships as `node -v` and is wrong for a uv or Cargo project;
  `external` says `cloudctl` is not a real program. A test asserts every
  template carries the line, and another renders `supabase` against the
  config `supabase init` actually writes, so the `find` strings cannot ship
  broken.
