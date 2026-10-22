- The Git endpoint's container now stops after 10 minutes without a request,
  taking a snapshot first when anything was composed since the last one, so
  the next fetch restores everything it had. A fetch still streaming keeps
  it awake. Before, nothing stopped it explicitly, and a sleep could leave
  up to 10 minutes of composed views out of the snapshot.
