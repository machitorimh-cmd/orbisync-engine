//! Explicit R3 exception: finite pinned SQLx encoding/framing is not claimed
//! to observe cancellation within 16,384 work. Observe around each driver poll
//! and yield before/after the operation; transaction ownership stays outside.
use super::{ApplicationError, WriterPermit, invalid};
use std::{
    future::{Future, poll_fn},
    pin::pin,
    task::Poll,
};

pub(super) async fn run<T>(
    writer: &WriterPermit,
    operation: impl Future<Output = Result<T, ApplicationError>>,
) -> Result<T, ApplicationError> {
    // The operation is lazy: query construction/BYTEA bind runs in its poll,
    // not while the caller constructs this future.
    tokio::task::yield_now().await;
    let mut operation = pin!(operation);
    let result = poll_fn(|cx| {
        if !writer.is_live() {
            return Poll::Ready(Err(invalid()));
        }
        let result = operation.as_mut().poll(cx);
        if !writer.is_live() {
            return Poll::Ready(Err(invalid()));
        }
        result
    })
    .await;
    // Yield on errors too, before caller unwinding/transaction cleanup.
    tokio::task::yield_now().await;
    if !writer.is_live() {
        return Err(invalid());
    }
    result
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    fn writer() -> WriterPermit {
        WriterPermit::new(orbisync_application::checkpoint_admission::WriterToken {
            epoch: 1,
            boot: uuid::Uuid::now_v7(),
        })
        .unwrap()
    }
    #[tokio::test]
    async fn r3_driver_pre_post_poll_cancellation() {
        let permit = writer();
        permit.invalidate();
        let calls = AtomicUsize::new(0);
        assert!(
            run(&permit, async {
                calls.fetch_add(1, Ordering::Relaxed);
                Ok(())
            })
            .await
            .is_err()
        );
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        let permit = writer();
        assert!(
            run(&permit, async {
                permit.invalidate();
                Ok(())
            })
            .await
            .is_err()
        );
        let permit = writer();
        assert!(
            run(
                &permit,
                poll_fn(|_| {
                    permit.invalidate();
                    Poll::<Result<(), ApplicationError>>::Pending
                })
            )
            .await
            .is_err()
        );
    }
    #[tokio::test]
    async fn r3_driver_post_yield_keeps_caller_owner() {
        struct Owner<'a>(&'a AtomicUsize);
        impl Drop for Owner<'_> {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        let drops = AtomicUsize::new(0);
        let owner = Owner(&drops);
        let permit = writer();
        let polls = AtomicUsize::new(0);
        let mut wait = Box::pin(run(&permit, async {
            let _borrow = &owner;
            polls.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }));
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(wait.as_mut().poll(&mut cx).is_pending()); // before operation
        assert_eq!(polls.load(Ordering::Relaxed), 0);
        assert!(wait.as_mut().poll(&mut cx).is_pending()); // after ready operation
        assert_eq!(polls.load(Ordering::Relaxed), 1);
        assert_eq!(drops.load(Ordering::Relaxed), 0);
        permit.invalidate();
        assert!(wait.await.is_err());
        assert_eq!(drops.load(Ordering::Relaxed), 0);
        drop(owner);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn r3_driver_error_and_interrupted_poll_drop_operation() {
        struct Operation<'a>(&'a AtomicUsize, bool);
        impl Future for Operation<'_> {
            type Output = Result<(), ApplicationError>;
            fn poll(
                self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
            ) -> Poll<Self::Output> {
                if self.1 {
                    Poll::Ready(Err(invalid()))
                } else {
                    Poll::Pending
                }
            }
        }
        impl Drop for Operation<'_> {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        for error in [false, true] {
            let permit = writer();
            let drops = AtomicUsize::new(0);
            let mut wait = Box::pin(run(&permit, Operation(&drops, error)));
            let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
            assert!(wait.as_mut().poll(&mut cx).is_pending()); // pre-yield
            assert!(wait.as_mut().poll(&mut cx).is_pending()); // pending or error post-yield
            assert_eq!(drops.load(Ordering::Relaxed), 0);
            drop(wait);
            assert_eq!(drops.load(Ordering::Relaxed), 1);
            assert!(permit.is_live()); // caller retains ownership; no detached operation
        }
    }
}
