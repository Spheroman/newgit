# Contributing to newgit

Thanks for helping. Before your first pull request can be merged, sign the
[Contributor License Agreement](CLA.md).

## Signing the CLA

In your first pull request, add one line to the end of
[`.github/cla/signatures.txt`](.github/cla/signatures.txt):

```
your-github-login | Your Full Name | 2026-10-30 | 1
```

The commit that adds the line is your signature. You only sign once; the CLA
check on every later pull request finds your line.

If you're contributing as part of your job, your employer may own what you
write. Check before you sign, and if they need to sign for you, open an
issue first.

## Why there is a CLA

The CLI is MIT or Apache-2.0, and the remote is AGPL-3.0-only. Some of newgit
is not published at all. The CLA lets the maintainers use a contribution
across all of that, and change a project's license later, without tracking
down every contributor. You keep the copyright in what you write.

## The rest

- New tests go in a new file under `tests/`, not at the end of an existing
  one.
- Each change adds one changelog fragment to `changelog.d/`; see
  [its README](changelog.d/README.md).
