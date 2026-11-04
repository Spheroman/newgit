- The remote caps how many projects one login may own
  (`PROJECTS_PER_LOGIN`, 5 by default), shuts out anyone in
  `BLOCKED_LOGINS` even when `ALLOWED_LOGINS` is `*`, and lets the
  logins in `ADMIN_LOGINS` delete any project.

  newgit.dev is now open to any GitHub account. Without these, anyone
  could create unlimited projects (each with its own repos and Git
  endpoint container) on the operator's bill, and nobody could remove
  them or keep the person out.
