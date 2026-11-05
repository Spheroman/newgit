- An error on the remote's side names the request it answered, and a
  fork's Artifacts error says what it was
  ([#144](https://github.com/Spheroman/newgit/issues/144)).

  A push that failed with "the newgit remote refused (500): internal error"
  gave no way to tell which of its requests failed. A 5xx now names the
  call (`on POST /v1/…/fork`), and every Artifacts call an instance's fork
  makes, the look-ups included, answers with Artifacts' code and message.
