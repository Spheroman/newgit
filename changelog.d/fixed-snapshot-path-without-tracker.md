- **`newgit resource check` fails `{{snapshot.path}}` with no
  `into_tracker`.** The directory exists only to be deposited in a tracker,
  so without one the checkpoint command ran with the literal text; `spawn`
  and `status` name it too.
