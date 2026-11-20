//! Which log a `status` problem points at: the run it is about, not only
//! the directory every run's log is in.

/// The newest log of `resource`'s `action` in an instance's logs
/// directory. Every run writes its own, `<resource>.<action>-<UTC
/// time>.log`, so the newest is the run a `failed` status is about.
pub(super) fn last_action_log(
    logs: &camino::Utf8Path,
    resource: &str,
    action: &str,
) -> Option<camino::Utf8PathBuf> {
    let prefix = format!("{resource}.{action}-");
    // The name sorts by time: the stamp is fixed-width and follows the
    // prefix.
    logs.read_dir_utf8()
        .ok()?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_owned())
        .filter(|name| name.starts_with(&prefix) && name.ends_with(".log"))
        .max()
        .map(|name| logs.join(name))
}

/// `its log is <path>` when the run's log is known, else the directory.
pub(super) fn last_log_line(log: Option<camino::Utf8PathBuf>, logs: &camino::Utf8Path) -> String {
    match log {
        Some(log) => format!("its log is {log}"),
        None => format!("its logs are under {logs}"),
    }
}
