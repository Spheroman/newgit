use std::collections::BTreeSet;
use std::net::TcpListener;

use crate::branch::BranchInstance;
use crate::error::{NewgitError, Result};

/// Ports already promised to instances. Binding records are the single
/// source of truth: removing an instance frees its ports with no ledger.
pub fn used_ports(branches: &[BranchInstance]) -> BTreeSet<u16> {
    branches
        .iter()
        .flat_map(|branch| branch.resources.values())
        .flat_map(|binding| binding.resolved_ports.values().copied())
        .collect()
}

/// First port scanning up from `start` that is neither promised to another
/// instance nor OS-unbindable right now. The chosen port is added to `used`
/// so one allocation pass stays self-consistent.
pub fn allocate(start: u16, used: &mut BTreeSet<u16>) -> Result<u16> {
    let mut candidate = start;
    loop {
        if !used.contains(&candidate) && bindable(candidate) {
            used.insert(candidate);
            return Ok(candidate);
        }
        candidate = candidate.checked_add(1).ok_or_else(|| {
            NewgitError::Unsupported(format!("no free port found scanning up from {start}"))
        })?;
        if candidate - start > 1000 {
            return Err(NewgitError::Unsupported(format!(
                "no free port found within 1000 of {start}"
            )));
        }
    }
}

fn bindable(port: u16) -> bool {
    TcpListener::bind(("127.0.0.1", port)).is_ok()
}
