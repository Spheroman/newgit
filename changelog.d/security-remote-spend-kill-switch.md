- The remote pauses itself for everyone but its admins while the last 7
  days' Cloudflare bill is over `SPEND_LIMIT_WEEKLY_USD` ($10 unless set),
  and reopens on its own once it falls back under.

  Cloudflare offers no hard spending cap, and newgit.dev is open to any
  GitHub account. An hourly cron reads the Billable Usage API (it needs a
  `BILLING_TOKEN` with Billing: Read); the numbers lag about a day, so this
  bounds a runaway rather than stopping at the cent.
