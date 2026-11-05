// newgit fork patch: this build installs beside upstream T3 Code, so nothing it owns may collide.
// The app ID and product name live in scripts/build-desktop-artifact.ts.
/** Under the home directory; upstream uses `.t3`. */
export const FORK_DATA_DIR = ".t3-newgit";
/** Electron profile under the OS app-data directory; upstream uses `t3code-v2`. */
export const FORK_USER_DATA_DIR = "t3code-newgit";
export const FORK_CLI_NAME = "t3code-newgit";
