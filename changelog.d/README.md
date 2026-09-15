# changelog.d

One file per change, instead of one `## [Unreleased]` section every branch
appends to. Different filenames cannot conflict, so six parallel branches
land without queueing behind whichever one got there first.

Add a fragment in the same commit as the change:

```
changelog.d/fixed-28-status-truth.md
changelog.d/added-50-install-store.md
changelog.d/removed-27-kind.md
```

The name is `<category>-<issue>-<slug>.md`. The category prefix is one of
`added`, `changed`, `deprecated`, `removed`, `fixed`, `security`,
`documentation` — it decides which `###` section the entry lands in, and CI
rejects a prefix that is not on that list rather than silently dropping the
entry at release time. The rest of the name is for humans; only uniqueness
matters. Drop the issue number if there isn't one.

The file holds exactly what the entry would have looked like in
`CHANGELOG.md`: one Markdown list item, continuation lines indented two
spaces, wrapped at 76 columns.

```markdown
- `status` no longer reports states that are not true
  ([#28](https://github.com/Spheroman/newgit/issues/28)).

  Why it was wrong and what it means for someone upgrading.
```

Write it when you make the change, and say *why* rather than *what* — that
is the whole value of these entries, and the reason the changelog is not
generated from PR titles at release time.

At release, `scripts/release-changelog.sh <version>` concatenates the
fragments into `CHANGELOG.md` under the new version heading, updates the
compare links, and deletes the fragments. `scripts/release-changelog.sh
--check` validates names without changing anything; CI runs it on every PR.
