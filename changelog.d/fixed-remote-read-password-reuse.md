- Git requests to a remote reuse a read password for its lifetime, where each
  request used to mint and record one per tracker, which was most of its time.
  What a caller may read is still decided on every request.
