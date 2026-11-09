- A step of a stepped push that Artifacts drops with an HTTP 500 is a
  smaller step, or the same one again, rather than the end of the push.
  Git says it as `The requested URL returned error: 500`, which newgit did
  not recognise as a drop, so copying `t3code` into an instance's fork
  died 43 minutes in. A one-commit step the remote keeps dropping is now
  tried a few more times before the push gives up.
