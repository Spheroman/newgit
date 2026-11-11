- A file of a tracker outside Git defined with project-relative `paths`
  inside another tracker's mount (`paths = ["apps/t3code/.env"]`, what
  `newgit tracker track` writes) is now refused like one defined with a
  `mount`. Before, a commit on an export target adding that file passed the
  check: `newgit pull` and `newgit pull --remote` took it in, overwriting
  the local secret and putting it into the tracker's history. One rule
  decides ownership for both spellings: a claim inside a deeper mount
  reaches it. The refusal on a pull now says the file is not taken in,
  rather than "refusing to send this work anywhere".
