- The Linux release binaries are static musl builds
  (`newgit-<arch>-unknown-linux-musl.tar.gz`), replacing the glibc builds.
  Those needed glibc 2.39, so they did not run on Debian 12, Ubuntu 22.04,
  RHEL 9, Amazon Linux 2023, or Alpine. The static builds run on all of them.
