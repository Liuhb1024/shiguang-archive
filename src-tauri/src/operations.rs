//! A command holds a read lease until all its work and cleanup finish.
//! Session changes cancel old futures, then wait for an exclusive lease.
use std::{future::Future, pin::pin, task::Poll};
use tokio::sync::{watch, RwLock, RwLockReadGuard, RwLockWriteGuard};

pub(crate) const CANCELLED: &str = "操作已取消，请在当前账号下重新操作";

pub(crate) struct OperationGate {
    lock: RwLock<()>,
    revision: watch::Sender<u64>,
}

impl OperationGate {
    pub(crate) fn new() -> Self {
        Self {
            lock: RwLock::new(()),
            revision: watch::channel(0).0,
        }
    }

    pub(crate) async fn read(&self) -> Operation<'_> {
        let cancel = self.revision.subscribe();
        let guard = self.lock.read().await;
        Operation {
            _guard: guard,
            cancel,
        }
    }

    pub(crate) fn cancel(&self) {
        self.revision
            .send_modify(|revision| *revision = revision.wrapping_add(1));
    }

    pub(crate) async fn transition(&self) -> RwLockWriteGuard<'_, ()> {
        self.cancel();
        self.lock.write().await
    }

    pub(crate) fn try_exclusive(&self) -> Result<RwLockWriteGuard<'_, ()>, String> {
        self.lock
            .try_write()
            .map_err(|_| "仍有任务或媒体读取进行中，请先停止并等待完成".into())
    }
}

pub(crate) struct Operation<'a> {
    _guard: RwLockReadGuard<'a, ()>,
    cancel: watch::Receiver<u64>,
}

impl Operation<'_> {
    pub(crate) async fn run<T>(
        &mut self,
        future: impl Future<Output = Result<T, String>>,
    ) -> Result<T, String> {
        let mut cancelled = pin!(self.cancel.changed());
        let mut work = pin!(future);
        std::future::poll_fn(|cx| {
            // Cancellation wins even when the work is already ready. Dropping this
            // future drops in-flight requests and temporary-file cleanup guards.
            if cancelled.as_mut().poll(cx).is_ready() {
                return Poll::Ready(Err(CANCELLED.to_owned()));
            }
            work.as_mut().poll(cx)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::Arc, time::Duration};

    #[test]
    fn core_fix_cancel_drops_inflight_future_and_allows_transition() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let gate = Arc::new(OperationGate::new());
                let mut operation = gate.read().await;
                let other = gate.clone();
                let transition = tokio::spawn(async move {
                    drop(other.transition().await);
                });
                let result = tokio::time::timeout(
                    Duration::from_millis(100),
                    operation.run(std::future::pending::<Result<(), String>>()),
                )
                .await;
                assert_eq!(result.unwrap(), Err(CANCELLED.to_owned()));
                assert!(!transition.is_finished()); // Cleanup must still hold the lease.
                drop(operation);
                transition.await.unwrap();
                assert!(gate.try_exclusive().is_ok());
            });
    }

    #[test]
    fn core_fix_cancel_before_poll_does_not_run_work() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let gate = OperationGate::new();
                let mut operation = gate.read().await;
                gate.cancel();
                let mut ran = false;
                let result = operation
                    .run(async {
                        ran = true;
                        Ok(())
                    })
                    .await;
                assert_eq!(result, Err(CANCELLED.to_owned()));
                assert!(!ran);
                drop(operation);
                let mut next = gate.read().await;
                assert_eq!(next.run(async { Ok(42) }).await, Ok(42));
            });
    }
}
