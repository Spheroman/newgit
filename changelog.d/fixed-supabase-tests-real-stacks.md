- The `supabase` template tests no longer bring up a real Supabase stack
  per spawned instance on a machine with the CLI and Docker.

  #290 stubbed `prepare` by matching `command = "supabase start"` as text.
  When #264 changed `prepare` to `supabase stop ...; supabase start`, the
  match landed on `[actions.resume]` instead, and the real `prepare` ran
  again. It started a dozen `EDIT_ME_PROJECT-feature-*` containers each
  run, and they outlived the tempdir. The second-instance test never had
  the stub. All three test files now share one stub that parses the
  definition, sets every action to `true`, and fails if any action still
  runs a real command.
