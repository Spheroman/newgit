- A file of a tracker outside Git defined with project-relative `paths`
  inside another tracker's mount (`paths = ["apps/t3code/.env"]`, what
  `newgit tracker track` writes) is now refused like one defined with a
  `mount`. Before, a commit on an export target adding that file passed the
  check: `newgit pull` and `newgit pull --remote` took it in, overwriting
  the local secret and putting it into the tracker's history. One rule
  decides ownership for both spellings, everywhere (the gates, split, the
  workspace): the longest claim that reaches a path owns it, and only a
  tracker outside Git's claim reaches into a deeper mount. A claim of a
  tracker in Git inside a deeper mount owns nothing, and `newgit tracker
  list` says so. The refusal on a pull now says the file is not taken in,
  rather than "refusing to send this work anywhere".
- Split no longer drops a tracker's longer claim inside a shorter claim of
  a tracker sharing its mount (`x/sec` inside another's `x`): the file
  stayed in the parent instead.
