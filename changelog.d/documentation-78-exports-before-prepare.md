- The reference now states that a resource's ports and `[exports]` are bound
  before its own `prepare` runs
  ([#78](https://github.com/Spheroman/newgit/issues/78)).

  `[exports]` was "rendered once at `spawn`" and `prepare` "runs on its own —
  at `spawn`". Both at `spawn`, with no order between them, which is not
  enough to write a `prepare` against. The within-resource order is now
  written down — allocate `[ports]`, render `[exports]`, apply `[[render]]`,
  then run `prepare` — along with the fact that a dependency's exports were
  bound an iteration earlier, in dependency order.

  The failure from guessing wrong is quiet: a `prepare` that brings up a
  Compose stack named by its own `{{branch.slug}}` export, run before that
  export existed, brings up the *shared* stack under the committed default
  name. No error, correct-looking output, wrong cluster. The workaround —
  restating the value inline in every hook that needs it — is the
  duplication this reference warns against elsewhere, and is no longer
  necessary. `tests/bind_order_and_stop_name.rs` pins both halves.
