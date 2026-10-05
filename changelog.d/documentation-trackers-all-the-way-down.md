- The README, the v1 spec, `newgit reference`, and the architecture and
  Cloudflare docs now describe the tracker model the code implements: `root`
  owns `.` and `project` owns `.newgit`; ownership nests, with the most
  specific claim winning; `secret` and code trackers (`tracker create --code
  --path`) replace the implicit `source` tracker; and the remote commands
  (`login`, `remote add`, `members`, `spawn --from-checkpoint`) are listed.
  The `--from-checkpoint` help no longer points at a `checkpoints --remote`
  flag that does not exist.
