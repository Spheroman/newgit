- `newgit pull --remote <target>` brings an export target's new commits (a
  pull request merged on GitHub, an upstream sync) back into its trackers
  ([#142](https://github.com/Spheroman/newgit/issues/142)).

  Before, the only way in from GitHub was `git pull` in the project root,
  which works only while the mirror's ids are the project's. For a target
  with `root`, the commits are the tracker's own: its `main` fast-forwards
  or really merges, and its pin in the project root moves as `newgit pull
  <tracker>` would, so old-dated upstream history comes in on top through
  one merge. For a target without `root`, commits that came from here are
  recognised by id, by the note on an exported commit, or by a canonical
  id, and the rest come into the project root as work, with the files of
  trackers the target withholds put back. Either way a file a tracker
  outside Git owns is refused, and `newgit merge main` publishes.
