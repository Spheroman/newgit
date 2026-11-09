# Overview

How newgit works and how to drive it, in more depth than the
[README](../../README.md). For every tracker and resource key, its default,
and which template variables each hook sees, run `newgit reference`: the
definition format travels with the binary.

The central idea shows up in the code shape: branch instances bind source
state to user-defined trackers and resources, instead of baking env files,
installs, processes, databases, or external resources into special internal
lanes.

## Using newgit

- [The workflow](workflow.md): the whole loop, on one machine and with a remote
- [Trackers](trackers.md): what owns which files, and what an instance binds
- [Adopting it in a project](adopting.md): `init`, what to commit, and what installs cost
- [Behavior worth knowing](behavior.md): export, GitHub, tags, `[[render]]`, `cleanup`
- [Commands](commands.md): every command, in one list

## With your agent

- [T3 Code](t3-code.md): a newgit instance for every thread
- [Claude Code](claude-code.md): a newgit instance for every worktree

## Working on newgit

- [Development](development.md): building, testing, and the repository's layout
- The design documents explain why it is shaped the way it is:
  [the design](../design/spec.md), [the tracker model](../design/trackers.md),
  and [the remote](../design/remote.md).

The wiki is the Markdown in
[`docs/wiki/`](https://github.com/Spheroman/newgit/tree/main/docs/wiki).
Every page has an edit link, and a change is live within a minute of merging.
