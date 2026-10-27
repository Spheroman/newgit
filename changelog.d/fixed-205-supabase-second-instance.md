- The `supabase` template allocates and renders every port `supabase init`
  writes, so a second instance's render is no longer refused
  ([#205](https://github.com/Spheroman/newgit/issues/205)).

  The template allocated six ports, but `supabase init` writes nine in the
  same 543xx block: the disabled pooler's `port = 54329` and the
  commented-out `# smtp_port = 54325` and `# pop3_port = 54326` as well.
  Once a first instance held the six, the second instance's scan landed on
  those numbers, and the render refused: `port = 54325` also matches inside
  `# smtp_port = 54325`, so the file would name the value twice and
  `newgit capture` could not reverse it. The template now has `smtp`,
  `pop3` and `pooler` lanes, rendered like the others, so those ports also
  move per instance once enabled or uncommented. A project that added the
  template before this has its own copy under `.newgit/resources/`, and
  `resource add` will not overwrite it. Copy the three `[ports]` lines and
  their three `[[render]]` rules from `newgit resource templates --show
  supabase` into that file.
