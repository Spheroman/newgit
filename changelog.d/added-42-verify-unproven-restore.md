- Checkpoint output says when a resource's `[restore]` has never completed
  on the instance, and `newgit checkpoint --verify <instance>` can prove it
  on demand
  ([#42](https://github.com/Spheroman/newgit/issues/42)).

  `[checkpoint]` runs the day it is written; `[restore]` runs the day it is
  needed, which by definition is the day the instance is already in
  trouble. Every checkpoint in between looked identical whether or not the
  restore behind it had ever actually worked. A resource whose `[restore]`
  is `command` or `recompute` and has never completed successfully on this
  instance now gets `— restore never exercised` on its checkpoint line.

  `newgit checkpoint --verify <instance>` proves it early instead of
  leaving that for the day of the real rollback: checkpoint, restore with a
  real `undo`, checkpoint again, and compare the two runs' state refs.
  Destructive and expensive on purpose — it stops and restarts whatever the
  instance's resources run, which is exactly why it is a separate,
  explicitly named command rather than something `checkpoint` does on its
  own, and why it prints what it is about to do before doing it.

  "Completed" and "verified" are deliberately different claims: an ordinary
  undo marks `[restore]` as proven the moment its command exits `0`, even
  though that alone does not confirm the resource landed on the state the
  checkpoint recorded. `--verify`'s state-ref comparison is the stronger
  check, and it can fail even when the weaker one would have passed.
