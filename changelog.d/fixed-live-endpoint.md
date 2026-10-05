- Fixes from the first live run against newgit.dev: `newgit remote add
  --project <name>` creates the project under that name; the credential
  helper answers for the Git endpoint when Git sends no path (as it does over
  HTTPS by default); and the endpoint's container authenticates its fetches
  from Artifacts.
