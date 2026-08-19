use super::super::lock;
use crate::filesystem::{ByteRange, ByteRangeSet};
use std::sync::Mutex;

use super::backend::ContentBackend;

pub(crate) struct ManagedContent {
    pub(crate) state: ContentState,
    pub(super) backend: Box<dyn ContentBackend>,
}

pub(crate) struct ContentState {
    pub(crate) writable: bool,
    pub(crate) dirty: Mutex<ByteRangeSet>,
    pub(crate) materialized: Mutex<ByteRangeSet>,
}

impl ManagedContent {
    pub(super) fn new(backend: impl ContentBackend + 'static, writable: bool) -> Self {
        Self {
            state: ContentState {
                writable,
                dirty: Mutex::new(ByteRangeSet::default()),
                materialized: Mutex::new(ByteRangeSet::default()),
            },
            backend: Box::new(backend),
        }
    }

    pub(crate) fn writable(&self) -> bool {
        self.state.writable
    }
}

impl ContentState {
    pub(super) fn record_write(&self, range: ByteRange) {
        if !self.writable {
            return;
        }
        lock(&self.materialized).insert(range);
        lock(&self.dirty).insert(range);
    }
}
