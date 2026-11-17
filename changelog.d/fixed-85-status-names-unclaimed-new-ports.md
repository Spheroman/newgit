- `newgit status` names a port declared after an instance bound its
  resource ([#85](https://github.com/Spheroman/newgit/issues/85)).

  Adding `inbucket` to a definition left every live instance's record
  claiming only the old ports while its tool used the new one, so the
  allocator's ledger was silently incomplete and could hand that port to
  another instance. `status` now reports such a resource as not ready,
  naming the ports, with `newgit resource rebind <resource>` as the fix.
