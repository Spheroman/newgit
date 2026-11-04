- A tracker definition gains `mount` (where its root sits in its parent,
  default `.`) and `follow` (the ref `newgit pull` brings in, default
  `main`); `paths` are claims relative to the mount and may exclude with
  `!` ([#201](https://github.com/Spheroman/newgit/issues/201)).

  Ownership follows the mounts: the deepest mount holding a path decides,
  then the longest claim among the trackers mounted there. A path every
  claim excludes is owned by nothing, and a commit changing it is refused
  when it leaves the machine, which is all "ignored" ever meant. Trackers
  sharing a mount split by their claims, so a tracker in Git mounted at
  `.newgit/trackers` beside `access` governs the definition files it claims;
  that replaces claiming `.newgit/trackers/<name>.toml` from `.`. Trackers
  outside Git may have a mount too; they are never pinned.
