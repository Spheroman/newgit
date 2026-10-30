- Instance names are limited to 40 characters, and `render --check` now
  reports environment-name collisions
  ([#263](https://github.com/Spheroman/newgit/issues/263)).

  A 300-character name used to fail with a raw "File name too long" from the
  lock file, and a 200-character one spawned and then broke Docker container
  names and Supabase `project_id`s built from `{{branch.slug}}`. The limit is
  40, which leaves 23 characters of prefix under a 63-character DNS label.
  `render --check` said nothing when two resources declared the same port
  `env` or export name; it now prints the same warning as `resource list`,
  from the same check `spawn` refuses on.
