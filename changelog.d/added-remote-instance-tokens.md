- Registering an instance with the remote returns that workspace's own
  token, so nobody creates tokens by hand: it writes only its own instance
  (fork, checkpoints, head), renews itself while the instance exists, and
  is revoked when the instance is removed.
