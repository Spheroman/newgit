- An unresolved `{{...}}` in `[exports]` refuses at spawn instead of being
  stored and shipped as a literal
  ([#40](https://github.com/Spheroman/newgit/issues/40)).

  Everywhere else an unknown placeholder renders verbatim, so the mistake is
  visible to whoever typed it. An export is the exception, for the same reason
  `[cleanup]` and `[[render]]` already refuse: it is rendered once, written
  into the binding record, and handed to every later action and `newgit run`
  as an environment variable. A bad command fails in front of the person who
  wrote it; a bad export surfaces in a different process, at whatever hour
  something first dials a host named `{{ports`.

  ```
  resource:  `functions` export: FAILED — resource `functions` leaves exports
  unresolved: `SUPABASE_FUNCTIONS_URL` ({{ports.supabase.api}}); ...
  ```

  Nothing that resource exports is stored, not just the value that failed: an
  absent environment variable is something downstream can detect, a malformed
  URL is not, and a binding that publishes half an environment is the same
  failure one variable further down — `newgit run` and every dependent's
  actions read a binding's exports without asking what status it holds. Every
  unresolved export is named at once, so fixing the first does not just reveal
  the second on the next spawn. Export failures are reported apart from render
  failures, because they are different mistakes in different parts of the
  definition.

  An export may compose a *sibling* as well as a dependency's export
  (`HEALTH_URL = "{{exports.BASE_URL}}/health"`). Composing the resource next
  door while the key two lines up was refused would have been a rule nobody
  could guess — and the refusal named a key that was defined right there. The
  table is a map with no declaration order to lean on (`HEALTH_URL` sorts
  first), so exports resolve to a fixed point instead: each pass renders what
  it can, and a pass that resolves nothing new ends it. A cycle stalls and is
  reported like any other placeholder that never resolved.
