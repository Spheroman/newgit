- Binding a resource again hands back the ports it had, so a resource
  whose `prepare` finds its old stack still up is not reported ready on
  ports nothing listens on
  ([#255](https://github.com/Spheroman/newgit/issues/255)).

  A drop used to release a resource's ports and the next `use` allocated
  new ones, re-rendered, and ran `prepare`; `supabase start` saw the old
  stack and exited 0 without reading the new config. Ports are for the
  instance's lifetime now: a parked binding keeps its claim (`newgit ports`
  shows it as `(parked)`, and `ports --check` counts it), and `use` brings
  the resource back on it, through `resume` when the drop ran `suspend`.
