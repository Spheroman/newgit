- A view commit rebuilt because its reader cannot see every tracker carries
  a note naming the commit it stands for (`canonical <id>`). The Git endpoint
  serves each view's notes as `refs/notes/commits` — only for commits in that
  view — so `git log` shows them once a clone fetches `refs/notes/*`.
