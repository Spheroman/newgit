- A merge below root now really lands at its anchor. The remote was working
  out the anchor at `begin`, before the client had sent its commits. It
  couldn't read them, so every merge landed at root, and two merges begun
  together still met there. The anchor is now worked out at the finish.
  `begin` compares the base only for the trackers the merge moves.
  - Approval is unchanged from before anchors: while `begin` can't read the
    commits, it still asks root's maintainers.
