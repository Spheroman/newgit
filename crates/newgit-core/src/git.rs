//! The one way newgit starts Git: [`command`].
//!
//! newgit decides what Git did partly from its messages: a lost race for a
//! ref is `not updating` or `cannot lock ref` on stderr, a fetch that met
//! another Git's repack is `shallow file has changed since we read it`. Git
//! translates those into the developer's language, so under a German or
//! Japanese locale the match never fires and a retry that should run fails
//! instead. Every Git newgit runs for itself runs in the C locale, so its
//! messages are the ones the code matches.
//!
//! `LC_ALL=C`, not `C.UTF-8` or `LC_MESSAGES=C`:
//!
//! - `LC_MESSAGES=C` loses to a developer's own `LC_ALL`, which outranks it.
//! - GNU gettext ignores `LANGUAGE` only when the message locale is exactly
//!   `C` or `POSIX`, so under `C.UTF-8` a developer's `LANGUAGE=de` would
//!   still translate. `LANGUAGE` is removed as well, for any gettext that
//!   reads it regardless.
//! - Git handles bytes the same in any locale: paths, commit messages and
//!   names are stored as given (`i18n.commitEncoding` defaults to UTF-8, not
//!   to the locale), `core.quotePath` quotes the same, refs sort by byte,
//!   and dates are formatted by Git, not `strftime`. Hooks Git runs inherit
//!   the C locale; newgit's own Git is plumbing, and Python, the usual hook
//!   language, coerces the C locale to UTF-8 (PEP 538).
//!
//! The Git a developer reads directly (`newgit log`, the `receive-pack` a
//! `git push` reaches) keeps their locale. Each such call says
//! `// git-locale:` above it, and `tests/git_command_sweep.rs` refuses any
//! other Git that does not come from here.

use std::process::Command;

/// `git`, with its messages in the C locale.
pub fn command() -> Command {
    let mut command = Command::new("git");
    command.env("LC_ALL", "C").env_remove("LANGUAGE");
    command
}
