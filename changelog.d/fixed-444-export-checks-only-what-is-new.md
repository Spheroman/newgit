- An export no longer refuses forever over one old commit
  ([#444](https://github.com/Spheroman/newgit/issues/444)). It checked a
  tracker's whole history for a file a tracker outside Git owns, so a
  commit that added `.env` before anything claimed it blocked every export
  after, and deleting the file did not help. Now only the commits the
  target does not have are checked, asked of the target at each push
  (never a record here, so a target force-pushed to purge a secret is not
  trusted to still have it); what it has is published already. A merge
  bringing published content beside new work passes; a commit the target
  lacks that adds such a file is still refused, and the error says the way
  out: rewrite the history without it on the target and `newgit pull
  --remote <target> --force`, or take the file out of the claim. An import
  is still checked whole (all of it is new to the project) and now names
  the same way out. The outside-Git check also lists a root commit's files
  whatever `log.showRoot` says.
