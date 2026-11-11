- The design now covers the disk that parallel instances pile up: the
  install store keeps an entry for every key an instance reaches and
  deduplicates a miss against the entries it already has
  ([#421](https://github.com/Spheroman/newgit/issues/421)), and `cleanup`
  names instances whose work has landed and can remove them
  ([#422](https://github.com/Spheroman/newgit/issues/422)).
