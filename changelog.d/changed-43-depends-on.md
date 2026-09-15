- `depends_on` means lifecycle again. Needing another resource's *value* is
  inferred from `{{exports.<name>}}` instead of declared, and orders binding
  without touching teardown
  ([#43](https://github.com/Spheroman/newgit/issues/43)).

  One key was driving two different claims. A `[[render]]` substituting
  another resource's URL into a config file needs that resource *bound*
  before it renders and nothing more — but the only key that produced that
  ordering also reversed into cleanup, so a pure data edge had to be written
  as a lifecycle edge and silently acquired teardown semantics it never asked
  for. The Supabase stack does not need the Expo dev server running, started,
  or ever used; it needs one string out of it. The graph asserted otherwise
  because there was no way to say the weaker thing.

  There still isn't a way to *say* it, and that is the fix: the template is
  already the statement. `{{exports.EXPO_URL}}` names what it needs, so the
  edge is read from it rather than restated:

  ```toml
  # supabase.toml — no depends_on
  [[render]]
  path = "packages/db/supabase/config.toml"
  replace = [
    { find = 'additional_redirect_urls = ["exp://127.0.0.1:8081"]',
      with = 'additional_redirect_urls = ["{{exports.EXPO_URL}}"]' },
  ]
  ```

  `web` binds first and the file renders correctly, with nothing declared in
  either direction. The graph now keeps two orders: bind order (`depends_on`
  plus data edges) for preparing, rendering, env assembly, and restore;
  lifecycle order (`depends_on` alone) reversed for checkpoint and cleanup.

  Inference costs legibility, so `newgit resource list` prints what it found,
  naming the export that caused each edge — an edge nobody wrote down has to
  be able to explain itself:

  ```
  Reads exports from (inferred from `{{exports.*}}`):
    supabase reads web (EXPO_URL)
    these order binding only — they say nothing about teardown
  ```

  Only bind-time consumers count: `[exports]` values and `[[render]]`
  replacements. `[cleanup]` and `[checkpoint]` may use `{{exports.*}}` too,
  but they read a binding record that is already complete, so there is
  nothing left to order. A name no resource exports is not an edge — it is a
  template that will not resolve, which `[exports]` and `[[render]]` already
  refuse with a better message. A resource referring to its own exports is
  the documented sibling case, not an edge to itself. A cycle through data
  edges alone is a real cycle and is reported like any other.

  This is what made the export-name rule below worth having first: reading an
  edge out of `{{exports.EXPO_URL}}` only works if exactly one resource can
  own that name.
