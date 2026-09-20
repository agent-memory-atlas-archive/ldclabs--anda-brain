//! Host time and owned background work. Neither is deserializable from a request.
#[cfg(feature = "experiments")]
use std::sync::atomic::{AtomicU64, Ordering};
use std::{future::Future, sync::Arc};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

#[derive(Default)]
pub(crate) struct BusinessClock {
    #[cfg(feature = "experiments")]
    manual: Option<AtomicU64>,
}

impl BusinessClock {
    #[cfg(feature = "experiments")]
    pub fn is_manual(&self) -> bool {
        self.manual.is_some()
    }
    pub fn now_ms(&self) -> u64 {
        #[cfg(feature = "experiments")]
        if let Some(clock) = &self.manual {
            return clock.load(Ordering::SeqCst);
        }
        anda_engine::unix_ms()
    }

    #[cfg(feature = "experiments")]
    pub fn manual(now_ms: u64) -> Result<Arc<Self>, anda_core::BoxError> {
        anda_engine::rfc3339_datetime(now_ms).ok_or("invalid business time")?;
        Ok(Arc::new(Self {
            manual: Some(AtomicU64::new(now_ms)),
        }))
    }

    #[cfg(feature = "experiments")]
    pub fn advance_to(&self, now_ms: u64) -> Result<(), anda_core::BoxError> {
        anda_engine::rfc3339_datetime(now_ms).ok_or("invalid business time")?;
        let clock = self
            .manual
            .as_ref()
            .ok_or("clock is not an experiment clock")?;
        clock
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |old| {
                (now_ms >= old).then_some(now_ms)
            })
            .map_err(|_| "business time cannot move backwards")?;
        Ok(())
    }

    /// Supply a default valid-time only. Explicit historical FOR TIME and
    /// AS OF stay intact; engine transaction/auth/lease clocks never change.
    #[allow(clippy::result_large_err)] // Preserve the native structured KIP error at the tool seam.
    pub fn bind_read(&self, request: &mut anda_kip::Request) -> Result<(), anda_kip::KipError> {
        #[cfg(feature = "experiments")]
        if self.manual.is_some() {
            let parsed = request.parse_operations()?;
            for (operation, mut command) in request.operations.iter_mut().zip(parsed) {
                if let anda_kip::Command::Kql(query) = &mut command
                    && query.for_time.is_none()
                {
                    query.for_time = Some(anda_kip::Scalar::Literal(anda_kip::KipValue::String(
                        crate::kip::timestamp(self.now_ms()),
                    )));
                    operation.command = None;
                    operation.ast = Some(command);
                }
            }
        }
        #[cfg(not(feature = "experiments"))]
        let _ = request;
        Ok(())
    }
}

type CancelHook = Arc<dyn Fn() + Send + Sync>;

/// Writes owned by the host survive a dropped API waiter. Shutdown closes
/// admission first, then drains admitted work without cancelling native commits.
#[derive(Clone, Default)]
pub(crate) struct DurableTasks {
    inner: Arc<DurableInner>,
}
struct DurableInner {
    closing: parking_lot::Mutex<bool>,
    tasks: TaskTracker,
    slots: Arc<tokio::sync::Semaphore>,
}
impl Default for DurableInner {
    fn default() -> Self {
        Self {
            closing: Default::default(),
            tasks: TaskTracker::new(),
            slots: Arc::new(tokio::sync::Semaphore::new(16)),
        }
    }
}
impl DurableTasks {
    pub fn is_busy(&self) -> bool {
        !self.inner.tasks.is_empty()
    }
    pub async fn run<T: Send + 'static>(
        &self,
        work: impl Future<Output = Result<T, anda_core::BoxError>> + Send + 'static,
    ) -> Result<T, anda_core::BoxError> {
        self.start(work)?.await?
    }
    pub fn start<T: Send + 'static>(
        &self,
        work: impl Future<Output = Result<T, anda_core::BoxError>> + Send + 'static,
    ) -> Result<tokio::task::JoinHandle<Result<T, anda_core::BoxError>>, anda_core::BoxError> {
        let handle = {
            let closing = self.inner.closing.lock();
            if *closing {
                return Err("attention runtime is closing".into());
            }
            let slot = self
                .inner
                .slots
                .clone()
                .try_acquire_owned()
                .map_err(|_| "attention operation queue is full")?;
            self.inner.tasks.spawn(async move {
                let _slot = slot;
                work.await
            })
        };
        Ok(handle)
    }
    pub async fn shutdown(&self) {
        {
            let mut closing = self.inner.closing.lock();
            *closing = true;
            self.inner.tasks.close();
        }
        self.inner.tasks.wait().await;
    }
}

#[derive(Clone, Default)]
pub(crate) struct RuntimeTasks {
    cancel: CancellationToken,
    tasks: TaskTracker,
    before_cancel: Arc<parking_lot::RwLock<Option<CancelHook>>>,
}

impl RuntimeTasks {
    #[cfg(feature = "experiments")]
    pub fn is_idle(&self) -> bool {
        self.tasks.is_empty()
    }
    pub fn set_cancel_hook(&self, hook: CancelHook) {
        *self.before_cancel.write() = Some(hook);
    }

    pub fn spawn(&self, work: impl Future<Output = ()> + Send + 'static) {
        let cancel = self.cancel.clone();
        let before_cancel = self.before_cancel.clone();
        self.tasks.spawn(async move {
            tokio::select! { biased; _ = cancel.cancelled() => {
                // Capture the current owner before dropping its processing
                // guard. Formation may have advanced to another queued id.
                let hook = before_cancel.read().clone();
                if let Some(hook) = hook { hook(); }
            }, _ = work => {} }
        });
    }

    pub fn cancel(&self) {
        let hook = self.before_cancel.read().clone();
        if let Some(hook) = hook {
            hook();
        }
        self.cancel.cancel();
        self.tasks.close();
    }

    pub async fn shutdown(&self) {
        self.cancel();
        self.tasks.wait().await;
    }
}
