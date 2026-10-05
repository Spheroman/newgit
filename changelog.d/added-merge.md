- `newgit merge [instance] [--into main]` merges an instance's committed work
  into a branch, tracker by tracker: unchanged trackers stay, one the branch
  has not moved on takes the instance's commit, one both sides changed gets a
  real Git merge in its own history, and any conflict moves nothing, naming
  the tracker and files. The branch's list then moves in one step — on the
  remote by compare-and-swap under a short merge lock, so a merge onto a list
  that moved meanwhile is refused. `newgit merge main` merges the project
  root. Merging is a person's decision: agent tokens cannot.
- Trackers carved, created, or redefined after `newgit remote add` reach the
  remote with the merge of the project root that made them: a new code
  tracker gets its repo then.
