- Export targets with `branches = true` receive each instance's branch
  every time its workspace pushes, and on a newgit remote the target
  becomes the workspace remote `gh` resolves.

  On a newgit remote, a workspace's `origin` is the newgit endpoint, so an
  agent's reflexive `git push` then `gh pr create` failed: `gh` cannot map
  that URL to a GitHub repository, and the branch never reached GitHub. Now
  the push exports the branch, the workspace's `refs/remotes/<target>/`
  holds the commit the target really has, and `gh pr create` works as is
  when the target ships every tracker the branch touches. When it does
  not, the commits there differ, and the push says `gh pr create --head
  <branch>` instead of pretending otherwise. A push straight to the
  target's remote is refused, since it would carry trackers the target
  withholds.
