- `gh` in a workspace resolves to the export target the instance's work
  belongs to, not the first `branches = true` one
  ([#143](https://github.com/Spheroman/newgit/issues/143)).

  With two targets that export branches, `gh pr create` used to open a pull
  request against whichever sorted first, even when the instance only changed
  trackers the other ships. Now a push resolves `gh` to the target that ships
  a tracker the work touched, and exports the branch only to such targets.
  When the work spans trackers shipped by more than one, the branch goes to
  each, `gh` resolves none, and the push names `gh pr create -R <repo>
  --head <branch>` for each. Pushing straight to a target is still refused.
