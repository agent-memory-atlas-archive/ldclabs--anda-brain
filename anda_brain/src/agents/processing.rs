use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64},
};

/// One admission lock for both writers. The atomics remain the public status
/// projections; taking a writer slot must happen under this lock.
#[derive(Default)]
pub(crate) struct ProcessingGate {
    pub admission: parking_lot::Mutex<()>,
    pub formation: Arc<AtomicU64>,
    pub maintenance: Arc<AtomicBool>,
}
