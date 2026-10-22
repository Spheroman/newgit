- The remote is invite-only. Only the GitHub logins in the Worker's
  `ALLOWED_LOGINS` secret (or anyone, with `*`) may log in or use a token,
  and the check runs on every request, so taking someone off the list shuts
  out the tokens they already hold. Unset admits no one: a deployment that
  forgot it is closed rather than open to every GitHub account.
