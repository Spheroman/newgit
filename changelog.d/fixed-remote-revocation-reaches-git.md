- Revoking a newgit token, or removing an instance, also revokes the repo
  passwords it was handed, so Git access ends at once instead of lasting up
  to 15 minutes. Renewal is the exception: it leaves them to expire, so a
  push in flight is not cut off.
