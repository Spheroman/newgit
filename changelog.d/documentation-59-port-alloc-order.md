- `[ports.<name>]` now says what order a resource's own ports are allocated
  in ([#59](https://github.com/Spheroman/newgit/issues/59)).

  `[ports]` is a map, not a sequence, and the reference only said how *one*
  port is chosen, not how a resource's several ports are ordered against
  each other. That matters as soon as two ports in one resource have ranges
  that can reach each other, which is the normal case for a tool whose
  defaults are consecutive. The allocation was already deterministic — a
  `BTreeMap`, iterated by key — and already matched display order, but
  nothing said so. The reference now states it, and a new test
  (`tests/port_alloc_order.rs`) pins name order against declaration order
  with two ports that share a start.
