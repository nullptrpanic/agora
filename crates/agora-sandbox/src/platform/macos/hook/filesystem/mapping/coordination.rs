use std::collections::VecDeque;
use std::sync::{Condvar, Mutex, MutexGuard};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AccessMode {
    Shared,
    Exclusive,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Resource {
    Descriptor(libc::c_int),
    Address { start: usize, end: usize },
    DescriptorRegistry,
    MappingRegistry,
}

impl Resource {
    fn overlaps(self, other: Self) -> bool {
        match (self, other) {
            (Self::Descriptor(left), Self::Descriptor(right)) => left == right,
            (
                Self::Address {
                    start: left_start,
                    end: left_end,
                },
                Self::Address {
                    start: right_start,
                    end: right_end,
                },
            ) => left_start < right_end && right_start < left_end,
            (Self::DescriptorRegistry, Self::DescriptorRegistry)
            | (Self::MappingRegistry, Self::MappingRegistry) => true,
            _ => false,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ResourceAccess {
    resource: Resource,
    mode: AccessMode,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct OperationRequest {
    accesses: Vec<ResourceAccess>,
}

impl OperationRequest {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn descriptor_shared(self, descriptor: libc::c_int) -> Self {
        self.with_access(Resource::Descriptor(descriptor), AccessMode::Shared)
    }

    pub(crate) fn descriptor_exclusive(self, descriptor: libc::c_int) -> Self {
        self.with_access(Resource::Descriptor(descriptor), AccessMode::Exclusive)
    }

    pub(crate) fn address_shared(self, start: usize, end: usize) -> Self {
        self.with_access(Resource::Address { start, end }, AccessMode::Shared)
    }

    pub(crate) fn address_exclusive(self, start: usize, end: usize) -> Self {
        self.with_access(Resource::Address { start, end }, AccessMode::Exclusive)
    }

    pub(crate) fn descriptor_registry_shared(self) -> Self {
        self.with_access(Resource::DescriptorRegistry, AccessMode::Shared)
    }

    pub(crate) fn descriptor_registry_exclusive(self) -> Self {
        self.with_access(Resource::DescriptorRegistry, AccessMode::Exclusive)
    }

    pub(crate) fn mapping_registry_shared(self) -> Self {
        self.with_access(Resource::MappingRegistry, AccessMode::Shared)
    }

    pub(crate) fn mapping_registry_exclusive(self) -> Self {
        self.with_access(Resource::MappingRegistry, AccessMode::Exclusive)
    }

    fn with_access(mut self, resource: Resource, mode: AccessMode) -> Self {
        if let Some(existing) = self
            .accesses
            .iter_mut()
            .find(|access| access.resource == resource)
        {
            if mode == AccessMode::Exclusive {
                existing.mode = AccessMode::Exclusive;
            }
        } else {
            self.accesses.push(ResourceAccess { resource, mode });
        }
        self
    }

    fn conflicts(&self, other: &Self) -> bool {
        self.accesses.iter().any(|left| {
            other.accesses.iter().any(|right| {
                left.resource.overlaps(right.resource)
                    && (left.mode == AccessMode::Exclusive || right.mode == AccessMode::Exclusive)
            })
        })
    }
}

struct Operation {
    id: u64,
    request: OperationRequest,
}

#[derive(Default)]
struct CoordinatorState {
    next_id: u64,
    active: Vec<Operation>,
    waiting: VecDeque<Operation>,
}

pub(crate) struct OperationCoordinator {
    state: Mutex<CoordinatorState>,
    changed: Condvar,
}

impl OperationCoordinator {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(CoordinatorState::default()),
            changed: Condvar::new(),
        }
    }

    pub(crate) fn acquire(&self, request: OperationRequest) -> OperationLease<'_> {
        let mut state = self.lock_state();
        let id = state.next_id;
        state.next_id = state
            .next_id
            .checked_add(1)
            .expect("operation coordinator identifier overflowed");
        state.waiting.push_back(Operation { id, request });

        loop {
            let position = state
                .waiting
                .iter()
                .position(|operation| operation.id == id)
                .expect("queued operation disappeared");
            let request = &state.waiting[position].request;
            let active_conflict = state
                .active
                .iter()
                .any(|operation| request.conflicts(&operation.request));
            let earlier_conflict = state
                .waiting
                .iter()
                .take(position)
                .any(|operation| request.conflicts(&operation.request));
            if !active_conflict && !earlier_conflict {
                let operation = state
                    .waiting
                    .remove(position)
                    .expect("grantable operation disappeared");
                state.active.push(operation);
                return OperationLease {
                    coordinator: self,
                    id,
                };
            }
            state = match self.changed.wait(state) {
                Ok(state) => state,
                Err(poisoned) => poisoned.into_inner(),
            };
        }
    }

    fn lock_state(&self) -> MutexGuard<'_, CoordinatorState> {
        match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    #[cfg(test)]
    pub(crate) fn waiting_count_for_test(&self) -> usize {
        self.lock_state().waiting.len()
    }

    #[cfg(test)]
    fn active_count_for_test(&self) -> usize {
        self.lock_state().active.len()
    }
}

pub(crate) struct OperationLease<'a> {
    coordinator: &'a OperationCoordinator,
    id: u64,
}

impl Drop for OperationLease<'_> {
    fn drop(&mut self) {
        let mut state = self.coordinator.lock_state();
        let position = state
            .active
            .iter()
            .position(|operation| operation.id == self.id)
            .expect("active operation disappeared");
        state.active.swap_remove(position);
        drop(state);
        self.coordinator.changed.notify_all();
    }
}

#[cfg(test)]
mod tests;
