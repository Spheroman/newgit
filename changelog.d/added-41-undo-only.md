- `newgit undo --only <resource>` restores one resource and leaves source,
  tracker content, and every other resource alone
  ([#41](https://github.com/Spheroman/newgit/issues/41)).

  Getting one resource's restore command right takes a few attempts, and
  rewinding six other resources each cycle is pure cost. A partial undo is not
  a snapshot the instance was ever in, so it never claims to be one:

  ```
  Restored `supabase` of `newgit-smoke` from ckpt_004
    source and tracker content left as they were (--only)
  ```

  It still takes a safety checkpoint first — it is still destructive — and it
  still stops and restarts the resources it touches, but only those.
