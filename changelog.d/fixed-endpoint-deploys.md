- A deploy now reaches a project's Git endpoint that had already slept.

  The endpoint came back from a snapshot of its whole container, old `newgit`
  binary and composed views included, so newgit.dev kept serving views
  composed by whatever `newgit` it first started with. A snapshot or running
  container from an earlier image is now dropped, and the endpoint starts
  fresh from the image deployed now.
