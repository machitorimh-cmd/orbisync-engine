use super::*;
use orbisync_application::checkpoint_admission::ReconciledAuthority;

#[tokio::test(start_paused = true)]
async fn cancelled_wait_releases_late_semaphore_permit() {
    let execution = Execution::new(tokio::time::Instant::now() + Duration::from_secs(5));
    let semaphore = Arc::new(Semaphore::new(0));
    let (send, entered) = tokio::sync::oneshot::channel();
    let task = tokio::spawn({
        let execution = execution.clone();
        let semaphore = semaphore.clone();
        async move {
            execution
                .scope(execution::wait(async move {
                    send.send(()).unwrap();
                    semaphore.acquire_owned().await.unwrap()
                }))
                .await
        }
    });
    entered.await.unwrap();
    execution.cancel();
    semaphore.add_permits(1);
    assert!(task.await.unwrap().is_err());
    assert_eq!(semaphore.available_permits(), 1);
}

#[tokio::test(start_paused = true)]
async fn deadline_ready_acquisition_releases_permit() {
    let execution = Execution::new(tokio::time::Instant::now() + Duration::from_secs(5));
    let semaphore = Arc::new(Semaphore::new(1));
    let acquired = AtomicBool::new(false);
    let result = execution
        .scope(execution::wait(async {
            // Complete acquisition at the deadline in the same poll: timeout may
            // accept a ready future even though the execution clock has expired.
            tokio::time::advance(Duration::from_secs(5)).await;
            let permit = semaphore.clone().acquire_owned().await.unwrap();
            acquired.store(true, Ordering::Release);
            permit
        }))
        .await;
    assert!(result.is_err());
    assert!(acquired.load(Ordering::Acquire));
    assert_eq!(semaphore.available_permits(), 1);
}

#[tokio::test(start_paused = true)]
async fn expired_conversion_wait_skips_factory_and_releases_job() {
    let store = Store::new();
    let service = store.service();
    let held = service.conversions.clone().acquire_owned().await.unwrap();
    let invoked = Arc::new(AtomicBool::new(false));
    let caller = tokio::spawn({
        let service = service.clone();
        let invoked = invoked.clone();
        let checkpoint = store.checkpoint.lock().unwrap().clone();
        async move {
            service
                .convert(
                    ReconciledAuthority {
                        source_id: None,
                        source_digest: None,
                        report: "deadline authority".into(),
                    },
                    move || {
                        invoked.store(true, Ordering::Release);
                        async move { Ok(checkpoint) }
                    },
                )
                .await
        }
    });
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(5)).await;
    drop(held);
    assert!(caller.await.unwrap().is_err());
    service.drain().await;
    assert!(!invoked.load(Ordering::Acquire));
    assert!(store.conversions.lock().unwrap().is_empty());
    assert!(service.pending_conversion_identity().is_none());
    assert_eq!(service.conversions.available_permits(), 1);
    assert_eq!(service.jobs.available_permits(), 2);
    assert_eq!(service.cleanup_pending_jobs(), 0);
}

#[tokio::test(start_paused = true)]
async fn cancelled_wait_releases_late_mutex_guard() {
    let execution = Execution::new(tokio::time::Instant::now() + Duration::from_secs(5));
    let mutex = Arc::new(tokio::sync::Mutex::new(()));
    let held = mutex.clone().lock_owned().await;
    let (send, entered) = tokio::sync::oneshot::channel();
    let task = tokio::spawn({
        let execution = execution.clone();
        let mutex = mutex.clone();
        async move {
            execution
                .scope(execution::wait(async move {
                    send.send(()).unwrap();
                    mutex.lock_owned().await
                }))
                .await
        }
    });
    entered.await.unwrap();
    execution.cancel();
    drop(held);
    assert!(task.await.unwrap().is_err());
    assert!(mutex.try_lock().is_ok());
}

#[tokio::test(start_paused = true)]
async fn live_and_standalone_waits_keep_acquired_ownership() {
    let execution = Execution::new(tokio::time::Instant::now() + Duration::from_secs(5));
    let semaphore = Arc::new(Semaphore::new(1));
    let permit = execution
        .scope(execution::wait(semaphore.clone().acquire_owned()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(semaphore.available_permits(), 0);
    drop(permit);
    let permit = execution::wait(semaphore.clone().acquire_owned())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(semaphore.available_permits(), 0);
    drop(permit);
    assert_eq!(semaphore.available_permits(), 1);
}

#[tokio::test(start_paused = true)]
async fn review80_cancelled_conversion_wait_must_not_enter_factory() {
    let store = Store::new();
    let service = store.service();
    let release = Arc::new(Semaphore::new(0));
    let (send, entered) = tokio::sync::oneshot::channel();
    let first = tokio::spawn({
        let service = service.clone();
        let release = release.clone();
        async move {
            service
                .inspect_legacy(move || async move {
                    send.send(()).unwrap();
                    release.acquire().await.unwrap().forget();
                    Ok(())
                })
                .await
        }
    });
    entered.await.unwrap();
    let invoked = Arc::new(AtomicBool::new(false));
    let second = tokio::spawn({
        let service = service.clone();
        let invoked = invoked.clone();
        let checkpoint = store.checkpoint.lock().unwrap().clone();
        async move {
            service
                .convert(
                    ReconciledAuthority {
                        source_id: None,
                        source_digest: None,
                        report: "review80 authority".into(),
                    },
                    move || async move {
                        invoked.store(true, Ordering::Release);
                        Ok(checkpoint)
                    },
                )
                .await
        }
    });
    tokio::task::yield_now().await;
    service.cancel_execution();
    release.add_permits(1);
    assert!(first.await.unwrap().is_err());
    assert!(second.await.unwrap().is_err());
    service.drain().await;
    assert!(store.conversions.lock().unwrap().is_empty());
    assert!(
        !invoked.load(Ordering::Acquire),
        "cancelled conversion invoked the queued factory"
    );
}
