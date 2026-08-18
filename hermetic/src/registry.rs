//! Linker-section registry of `HermeticState` impls: each emits a
//! `#[link_section = ".hermetic_state_registry"] static` of type
//! `HermeticVTable`, which the linker concatenates into a contiguous slice.

use slopos_ostd::{AllocError, KVec};

// The vtable lives in OSTD so `hermetic_state!` and the legacy
// `register_hermetic_state!` write one canonical type into the section.
pub use slopos_ostd::test_support::hermetic::HermeticVTable;

/// Iterate every registered `HermeticVTable` in linker order, which depends on
/// translation-unit link order; `topo_order` is the capture-order source of
/// truth.
pub fn registry_iter() -> impl Iterator<Item = &'static HermeticVTable> {
    slopos_ostd::ffi::registry::registry_slice::<HermeticVTable>(
        slopos_ostd::ffi::registry::RegistryId::HermeticStates,
    )
    .iter()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistryError {
    /// `DEPENDS_ON` graph contains a cycle.
    CycleDetected,
    /// A `DEPENDS_ON` entry names a state not present in the registry.
    MissingDep,
    /// Snapshot allocation failed during enter.
    Alloc,
}

impl From<AllocError> for RegistryError {
    fn from(_: AllocError) -> Self {
        Self::Alloc
    }
}

/// Topo-sort the registry by `DEPENDS_ON`: predecessors first, then
/// dependents. The scope captures snapshots in this order and restores in
/// reverse. `O(N²)` name lookup, bounded by N ≤ ~20 registered states.
pub fn topo_order() -> Result<KVec<&'static HermeticVTable>, RegistryError> {
    let entries: KVec<&'static HermeticVTable> = registry_iter().collect();
    let n = entries.len();
    let mut in_degree: KVec<usize> = (0..n).map(|_| 0usize).collect();

    // TODO(tech-debt): `in_degree` is written but never read — drop the counter
    // or drive the emit loop below from it.
    for (i, vt) in entries.iter().enumerate() {
        for dep_name in vt.depends_on {
            let mut found = false;
            for other in entries.iter() {
                if other.name == *dep_name {
                    found = true;
                    break;
                }
            }
            if !found {
                return Err(RegistryError::MissingDep);
            }
            in_degree[i] += 1;
        }
    }

    let mut result: KVec<&'static HermeticVTable> = KVec::new();
    let mut emitted: KVec<bool> = (0..n).map(|_| false).collect();

    loop {
        let mut progress = false;
        for (i, vt) in entries.iter().enumerate() {
            if emitted[i] {
                continue;
            }
            let mut all_deps_ready = true;
            for dep_name in vt.depends_on {
                let mut dep_emitted = false;
                for (j, other) in entries.iter().enumerate() {
                    if other.name == *dep_name && emitted[j] {
                        dep_emitted = true;
                        break;
                    }
                }
                if !dep_emitted {
                    all_deps_ready = false;
                    break;
                }
            }
            if all_deps_ready {
                let _ = result.push(*vt);
                emitted[i] = true;
                progress = true;
            }
        }
        if !progress {
            break;
        }
    }

    if result.len() != n {
        return Err(RegistryError::CycleDetected);
    }

    Ok(result)
}
