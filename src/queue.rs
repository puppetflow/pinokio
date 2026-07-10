use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::errors::GatewayError;

/// Concurrency gate combining the active session limit and the FIFO queue.
///
/// Design notes:
/// - `tokio::sync::Semaphore` serves waiters in FIFO order, which gives us
///   the fair queue for free and makes it impossible to bypass.
/// - The queue length is tracked with an atomic counter updated through a
///   compare-and-swap loop, so admission control is race-free: once
///   `max_queue_length` waiters are registered, further requests are
///   rejected immediately with 429.
/// - Permits are returned as `OwnedSemaphorePermit`, so a slot is released
///   exactly once when the session drops it, even on panic.
pub struct Gate {
    semaphore: Arc<Semaphore>,
    queued: Arc<AtomicUsize>,
    max_sessions: usize,
    max_queue: usize,
}

/// RAII guard that keeps the queue counter accurate even if the waiting
/// request is cancelled (client disconnect, timeout, shutdown).
struct QueueSlot {
    queued: Arc<AtomicUsize>,
}

impl Drop for QueueSlot {
    fn drop(&mut self) {
        self.queued.fetch_sub(1, Ordering::AcqRel);
    }
}

impl Gate {
    pub fn new(max_sessions: usize, max_queue: usize) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(max_sessions)),
            queued: Arc::new(AtomicUsize::new(0)),
            max_sessions,
            max_queue,
        }
    }

    /// Number of permits currently held (sessions starting or active).
    pub fn active_sessions(&self) -> usize {
        self.max_sessions
            .saturating_sub(self.semaphore.available_permits())
    }

    pub fn queued_sessions(&self) -> usize {
        self.queued.load(Ordering::Acquire)
    }

    pub fn max_sessions(&self) -> usize {
        self.max_sessions
    }

    pub fn max_queue(&self) -> usize {
        self.max_queue
    }

    /// True when a new request would be admitted right now (free slot or
    /// free queue spot).
    pub fn has_capacity(&self) -> bool {
        self.semaphore.available_permits() > 0 || self.queued_sessions() < self.max_queue
    }

    /// Acquires a session slot, waiting in the FIFO queue when none is free.
    ///
    /// Returns 429 immediately when the queue is full, 504 when
    /// `queue_timeout` expires, and 503 when the server starts shutting
    /// down while the request is waiting.
    pub async fn acquire(
        &self,
        queue_timeout: Duration,
        shutdown: &CancellationToken,
    ) -> Result<OwnedSemaphorePermit, GatewayError> {
        // Fast path: a slot is free, skip queue accounting entirely.
        if let Ok(permit) = Arc::clone(&self.semaphore).try_acquire_owned() {
            return Ok(permit);
        }

        // Register as a waiter, refusing when the queue is already full.
        let mut current = self.queued.load(Ordering::Acquire);
        loop {
            if current >= self.max_queue {
                return Err(GatewayError::QueueFull);
            }
            match self.queued.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
        let _slot = QueueSlot {
            queued: Arc::clone(&self.queued),
        };

        tokio::select! {
            permit = Arc::clone(&self.semaphore).acquire_owned() => {
                permit.map_err(|_| GatewayError::ShuttingDown)
            }
            _ = tokio::time::sleep(queue_timeout) => Err(GatewayError::QueueTimeout),
            _ = shutdown.cancelled() => Err(GatewayError::ShuttingDown),
        }
    }
}
