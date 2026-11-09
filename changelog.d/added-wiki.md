- newgit.dev has a wiki at [`/docs`](https://newgit.dev/docs), in the
  landing page's style, and the landing page is its home: both carry the
  same top bar and sidebar. Its pages are the Markdown in `docs/wiki/`,
  which replaces `docs/guide.md`: one page per section, readable on GitHub
  too, and live within a minute of merging. The Worker's `LANDING_PAGE`
  setting is now `SITE`, which names the landing page, the wiki's
  directory, and the template its pages are poured into.
