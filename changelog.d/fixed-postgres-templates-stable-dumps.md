- The bundled Postgres templates (`command-snapshot`,
  `command-snapshot-migrations`, `supabase`) now checkpoint to the same
  bytes for the same database, so `checkpoint --verify` can pass on them.

  Since 17.6 (and 16.10, 15.14, 14.19, 13.22), `pg_dump` wraps every plain
  dump in `\restrict` / `\unrestrict` with a fresh random key, so two dumps
  of an unchanged database never matched and every verify failed. The two
  host-`pg_dump` templates pass `--restrict-key=newgit`; `supabase`, whose
  CLI cannot forward that flag, swaps the random key for the same fixed one
  after dumping. A fixed key is a known key: it gives up pg_dump's defence
  against a hostile *server* injecting psql meta-commands into the dump.
  The server here is the instance's own local database, and the template
  comments say so. Existing resource files copied from these templates are
  not changed — add the flag by hand.
