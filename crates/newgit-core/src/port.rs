use std::collections::{BTreeMap, BTreeSet};

use sha2::{Digest, Sha256};

use crate::tracker::TrackerDefinition;

pub fn allocate_ports(
    branch_name: &str,
    definitions: &[TrackerDefinition],
) -> BTreeMap<String, BTreeMap<String, u16>> {
    let mut allocated = BTreeMap::new();
    let mut used = BTreeSet::new();
    let offset = branch_offset(branch_name);

    for definition in definitions {
        let mut tracker_ports = BTreeMap::new();
        for (name, request) in &definition.ports {
            let mut candidate = request.start.saturating_add(offset);
            while used.contains(&candidate) {
                candidate = candidate.saturating_add(1);
            }
            used.insert(candidate);
            tracker_ports.insert(name.clone(), candidate);
        }
        allocated.insert(definition.name.clone(), tracker_ports);
    }

    allocated
}

fn branch_offset(branch_name: &str) -> u16 {
    let digest = Sha256::digest(branch_name.as_bytes());
    let raw = u16::from_be_bytes([digest[0], digest[1]]);
    raw % 500
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::tracker::{Ownership, PortRequest, Propagation, TrackerDefinition};

    use super::allocate_ports;

    #[test]
    fn allocation_is_stable_for_branch_name() {
        let mut ports = BTreeMap::new();
        ports.insert(
            "app".to_owned(),
            PortRequest {
                start: 3100,
                env: Some("PORT".to_owned()),
            },
        );

        let definitions = vec![TrackerDefinition {
            name: "web".to_owned(),
            kind: "process".to_owned(),
            ownership: Ownership::Branch,
            propagation: Propagation::Pin,
            depends_on: Vec::new(),
            identity: None,
            materialize: None,
            ports,
            actions: BTreeMap::new(),
            capture: None,
            restore: None,
            cleanup: None,
            exports: BTreeMap::new(),
        }];

        assert_eq!(
            allocate_ports("auth-refactor", &definitions),
            allocate_ports("auth-refactor", &definitions)
        );
    }
}
