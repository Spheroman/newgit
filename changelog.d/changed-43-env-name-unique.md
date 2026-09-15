- An environment variable name may be declared only once across the project;
  a second claim is a graph problem
  ([#43](https://github.com/Spheroman/newgit/issues/43)).

  Three things declare a name — an `[exports]` key, an action's `captures`
  entry, and a port's `env` — and the command environment used to be
  assembled by layering them, exports in dependency order with port `env`
  vars over the top. So a name claimed twice resolved to whichever
  declaration happened to come last, and the loser was simply *absent* from
  the process that needed it, with nothing anywhere saying why. Both
  declarations look correct in their own file; the fault only exists in the
  union, which is not a thing you can read.

  Nothing wanted that behavior. It was never an override feature, just what
  fell out of building a map — and it was not even consistent, since port
  `env` vars beat exports regardless of dependency order while exports beat
  each other according to it. The set of names is fully known from the
  definitions, so the collision is now reported when the graph loads:

  ```
  environment variable `EXPO_URL` is declared more than once (`metro`
  [exports], `supabase` [exports]); a name may have only one owner — rename
  all but one, and compose it elsewhere with `{{exports.EXPO_URL}}`
  ```

  It is gated like any other graph problem — warned by the commands that
  *build* the graph, refused by `spawn`, `run`, `action`, `checkpoint`, and
  `undo` — so a project cannot be bricked by one, and `newgit resource list`
  still tells you where it is.

  The starter templates had to change with it. Adding the same template twice
  — a web and an api — is the canonical setup, and shipping conventional
  names meant the second `newgit resource add --template process` claimed the
  `PORT` and `APP_URL` the first already owned and refused the whole graph.
  That is newgit's own template breaking the project, not a user mistake, so
  templates now name their variables after the resource: `resource add web
  --template process` writes `WEB_PORT` and `WEB_URL`. The generated file
  says to rename them if you have one service and your tool insists on
  `PORT`. Two services in one project never could both publish it — every
  resource's environment lands in one process environment — so the
  conventional name was a promise templates could not keep.

  One overlap is still allowed, because it has one owner: a `captures` entry
  naming its own resource's `[exports]` key. The export states the value the
  definition knows up front and the action overwrites it with the one that
  did not exist until it ran.

  `NEWGIT_BRANCH` and `NEWGIT_WORKSPACE` are reserved for the same reason.
  newgit sets them last for every command it runs, so a resource declaring
  one could never reach the process — reported against the single claimant
  rather than passed over in silence.
