- The bundled database templates say what "up" means and fail loudly
  where they used to pass quietly.

  `supabase` has a `[ready]` check, a GET of Auth's health endpoint through
  Kong that needs no key, so something that `depends_on` it waits for the
  stack and `status` stops calling a stack that went down `ready`.
  `command-snapshot` and `command-snapshot-migrations` checked nothing at
  spawn: `createdb … || true` passed with the server down, and the instance
  read `ready` with no database. They now connect or create, and check the
  server with `pg_isready`. Every restore that loads a dump stops on the
  first error instead of reporting a half-loaded database as restored, and
  `command-snapshot`'s restore ends sessions still open on the database
  rather than failing the drop. The `process` template carries a commented
  `[ready] http` for a dev server that binds before it serves.
