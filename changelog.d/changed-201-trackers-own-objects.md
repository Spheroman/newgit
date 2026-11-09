- Every tracker on the remote keeps its own heads and log, in a Durable
  Object named by its path, such as `spheroman/newgit/t3code/repos`
  ([#201](https://github.com/Spheroman/newgit/issues/201)). A project's
  object keeps only what a project owns.
  - A change to one tracker is one compare-and-swap in that tracker's own
    object.
  - A change across several trackers reserves each, then commits them all
    at once, so no read sees half of it.
  - Existing projects move over by themselves, the first time each head is
    read.
- `git clone https://newgit.dev/<owner>/<name>/<path>.git` clones the view
  rooted at the tracker at that path, for example `/spheroman/newgit/t3code.git`.
