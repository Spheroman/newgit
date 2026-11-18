- A port's instances are allocated 100 apart, `start`, `start + 100`,
  `start + 200`, instead of one apart.

  A claim holds nothing until the instance's server binds it, and only
  newgit respects one. Dev tools that pick an extra port for themselves
  scan up from a default (`remix dev` takes the first free one in
  3001-3100), so with three instances on 3001-3003 the first one's tool
  took the second's port before its server was up, and only one app came
  up. A hundred apart, the ports just above each claim belong to no other
  instance. The reference and the `process` template now tell an agent to
  declare every port a tool opens in `[ports]` and pass it explicitly,
  which is the real fix; existing instances keep the ports they have.
