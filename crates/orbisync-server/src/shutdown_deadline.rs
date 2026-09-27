//! One signal-entry clock encompassing persistence, sockets and terminal cleanup.
use std::{future::Future, pin::Pin, time::Duration};
use tokio::{sync::oneshot, time::Instant};

pub(crate) enum Outcome<T> {
    Complete(T),
    Uncertain(&'static str),
}

// Borrow the completion future: even a forced result does not consume/drop its
// only owner. Production immediately terminates nonzero; tests retain/release it.
pub(crate) async fn coordinate<T>(
    mut work: Pin<&mut impl Future<Output = T>>,
    mut entry: oneshot::Receiver<Instant>,
    second_signal: impl Future<Output = ()>,
    drain: Duration,
    force: Duration,
    mut begin_force: impl FnMut(),
) -> Outcome<T> {
    let entered = tokio::select! {
        biased;
        result = &mut work => return Outcome::Complete(result),
        entered = &mut entry => match entered {
            Ok(entered) => entered,
            Err(_) => return Outcome::Uncertain("shutdown entry clock lost"),
        },
    };
    tokio::pin!(second_signal);
    let drain_deadline = entered + drain;
    let force_deadline = drain_deadline + force;
    tokio::select! {
        biased;
        () = &mut second_signal => return Outcome::Uncertain("second termination signal"),
        () = tokio::time::sleep_until(drain_deadline) => begin_force(),
        result = &mut work => return Outcome::Complete(result),
    }
    tokio::select! {
        biased;
        () = &mut second_signal => Outcome::Uncertain("second termination signal"),
        () = tokio::time::sleep_until(force_deadline) => Outcome::Uncertain("force deadline exceeded"),
        result = &mut work => Outcome::Complete(result),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    #[tokio::test(start_paused = true)]
    async fn admission88_pending_admission_keeps_original_force_deadline_and_owner() {
        let shutdown = orbisync_server::shutdown::ShutdownState::new();
        let accepted = shutdown.admit().expect("accepted before begin");
        shutdown.begin();
        assert!(shutdown.admit().is_none());
        let start = Instant::now();
        let (send, entry) = oneshot::channel();
        send.send(start).expect("entry");
        let work = shutdown.drain_admissions();
        tokio::pin!(work);
        let forced = AtomicBool::new(false);
        let outcome = coordinate(
            work.as_mut(),
            entry,
            std::future::pending(),
            Duration::from_secs(30),
            Duration::from_secs(10),
            || {
                assert_eq!(Instant::now() - start, Duration::from_secs(30));
                forced.store(true, Ordering::Release);
            },
        )
        .await;
        assert!(matches!(
            outcome,
            Outcome::Uncertain("force deadline exceeded")
        ));
        assert_eq!(Instant::now() - start, Duration::from_secs(40));
        assert!(forced.load(Ordering::Acquire));
        // Observation did not cancel or release the accepted operation.
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(work.as_mut().poll(cx).is_pending()))
                .await
        );
        drop(accepted);
        work.await;
    }

    #[tokio::test(start_paused = true)]
    async fn deadline78_shutdown_final_persist() {
        let started = Instant::now();
        let (send, entry) = oneshot::channel();
        assert!(send.send(started).is_ok());
        let witness = Arc::new(());
        let weak = Arc::downgrade(&witness);
        let (release, receive) = oneshot::channel();
        let work = async move {
            let _held = witness;
            let _released = receive.await;
        };
        tokio::pin!(work);
        let forced = AtomicBool::new(false);
        let result = coordinate(
            work.as_mut(),
            entry,
            std::future::pending(),
            Duration::from_secs(30),
            Duration::from_secs(10),
            || {
                assert_eq!(Instant::now() - started, Duration::from_secs(30));
                forced.store(true, Ordering::Release);
            },
        )
        .await;
        assert!(matches!(
            result,
            Outcome::Uncertain("force deadline exceeded")
        ));
        assert!(forced.load(Ordering::Acquire));
        assert_eq!(Instant::now() - started, Duration::from_secs(40));
        assert!(weak.upgrade().is_some());
        assert!(release.send(()).is_ok());
        work.await;
        assert!(weak.upgrade().is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn deadline78_second_signal_during_persistence() {
        let started = Instant::now();
        let (send, entry) = oneshot::channel();
        assert!(send.send(started).is_ok());
        let work = std::future::pending::<()>();
        tokio::pin!(work);
        let result = coordinate(
            work.as_mut(),
            entry,
            tokio::time::sleep(Duration::from_secs(1)),
            Duration::from_secs(30),
            Duration::from_secs(10),
            || {},
        )
        .await;
        assert!(matches!(
            result,
            Outcome::Uncertain("second termination signal")
        ));
        assert_eq!(Instant::now() - started, Duration::from_secs(1));
    }
}
