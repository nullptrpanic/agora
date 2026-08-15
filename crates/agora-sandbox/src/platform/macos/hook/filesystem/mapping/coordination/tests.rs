use super::*;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::Barrier;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

const SHORT_WAIT: Duration = Duration::from_millis(50);
const LONG_WAIT: Duration = Duration::from_secs(2);

fn wait_until_waiting(coordinator: &OperationCoordinator, count: usize) {
    let deadline = Instant::now() + LONG_WAIT;
    while coordinator.waiting_count_for_test() != count {
        assert!(
            Instant::now() < deadline,
            "coordinator waiter was not queued"
        );
        std::thread::yield_now();
    }
}

#[test]
fn descriptor_access_uses_shared_and_exclusive_conflicts() {
    let coordinator = Arc::new(OperationCoordinator::new());
    let exclusive = coordinator.acquire(OperationRequest::new().descriptor_exclusive(7));

    std::thread::scope(|scope| {
        let (started, started_rx) = mpsc::sync_channel(1);
        let (acquired, acquired_rx) = mpsc::sync_channel(1);
        let worker = Arc::clone(&coordinator);
        scope.spawn(move || {
            started.send(()).unwrap();
            let _lease = worker.acquire(OperationRequest::new().descriptor_shared(7));
            acquired.send(()).unwrap();
        });

        started_rx.recv_timeout(LONG_WAIT).unwrap();
        assert!(matches!(
            acquired_rx.recv_timeout(SHORT_WAIT),
            Err(RecvTimeoutError::Timeout)
        ));
        drop(exclusive);
        acquired_rx.recv_timeout(LONG_WAIT).unwrap();
    });

    let shared = coordinator.acquire(OperationRequest::new().descriptor_shared(7));
    std::thread::scope(|scope| {
        let (acquired, acquired_rx) = mpsc::sync_channel(1);
        let worker = Arc::clone(&coordinator);
        scope.spawn(move || {
            let _lease = worker.acquire(OperationRequest::new().descriptor_shared(7));
            acquired.send(()).unwrap();
        });
        acquired_rx.recv_timeout(LONG_WAIT).unwrap();
    });
    drop(shared);
}

#[test]
fn unrelated_descriptors_and_disjoint_ranges_do_not_conflict() {
    let coordinator = Arc::new(OperationCoordinator::new());
    let descriptor = coordinator.acquire(OperationRequest::new().descriptor_exclusive(7));
    let range = coordinator.acquire(OperationRequest::new().address_exclusive(0x1000, 0x2000));

    std::thread::scope(|scope| {
        let (acquired, acquired_rx) = mpsc::sync_channel(2);
        let first = acquired.clone();
        let descriptor_worker = Arc::clone(&coordinator);
        scope.spawn(move || {
            let _lease = descriptor_worker.acquire(OperationRequest::new().descriptor_exclusive(8));
            first.send(()).unwrap();
        });
        let range_worker = Arc::clone(&coordinator);
        scope.spawn(move || {
            let _lease =
                range_worker.acquire(OperationRequest::new().address_exclusive(0x2000, 0x3000));
            acquired.send(()).unwrap();
        });

        acquired_rx.recv_timeout(LONG_WAIT).unwrap();
        acquired_rx.recv_timeout(LONG_WAIT).unwrap();
    });

    drop(range);
    drop(descriptor);
}

#[test]
fn overlapping_ranges_and_registry_barriers_conflict() {
    let coordinator = Arc::new(OperationCoordinator::new());
    let range = coordinator.acquire(OperationRequest::new().address_shared(0x1000, 0x3000));
    let registry = coordinator.acquire(OperationRequest::new().descriptor_registry_shared());

    std::thread::scope(|scope| {
        let (range_acquired, range_rx) = mpsc::sync_channel(1);
        let (registry_acquired, registry_rx) = mpsc::sync_channel(1);
        let range_worker = Arc::clone(&coordinator);
        scope.spawn(move || {
            let _lease =
                range_worker.acquire(OperationRequest::new().address_exclusive(0x2000, 0x4000));
            range_acquired.send(()).unwrap();
        });
        let registry_worker = Arc::clone(&coordinator);
        scope.spawn(move || {
            let _lease =
                registry_worker.acquire(OperationRequest::new().descriptor_registry_exclusive());
            registry_acquired.send(()).unwrap();
        });

        assert!(matches!(
            range_rx.recv_timeout(SHORT_WAIT),
            Err(RecvTimeoutError::Timeout)
        ));
        assert!(matches!(
            registry_rx.recv_timeout(SHORT_WAIT),
            Err(RecvTimeoutError::Timeout)
        ));
        drop(range);
        drop(registry);
        range_rx.recv_timeout(LONG_WAIT).unwrap();
        registry_rx.recv_timeout(LONG_WAIT).unwrap();
    });
}

#[test]
fn earlier_conflicting_writer_is_not_bypassed_by_later_reader() {
    let coordinator = Arc::new(OperationCoordinator::new());
    let initial_reader = coordinator.acquire(OperationRequest::new().descriptor_shared(7));

    std::thread::scope(|scope| {
        let (order, order_rx) = mpsc::sync_channel(2);
        let (release_writer, release_writer_rx) = mpsc::sync_channel(1);
        let writer_order = order.clone();
        let writer_coordinator = Arc::clone(&coordinator);
        scope.spawn(move || {
            let _lease =
                writer_coordinator.acquire(OperationRequest::new().descriptor_exclusive(7));
            writer_order.send("writer").unwrap();
            release_writer_rx.recv_timeout(LONG_WAIT).unwrap();
        });
        wait_until_waiting(&coordinator, 1);

        let reader_coordinator = Arc::clone(&coordinator);
        scope.spawn(move || {
            let _lease = reader_coordinator.acquire(OperationRequest::new().descriptor_shared(7));
            order.send("reader").unwrap();
        });
        wait_until_waiting(&coordinator, 2);

        drop(initial_reader);
        assert_eq!(order_rx.recv_timeout(LONG_WAIT).unwrap(), "writer");
        assert!(matches!(
            order_rx.recv_timeout(SHORT_WAIT),
            Err(RecvTimeoutError::Timeout)
        ));
        release_writer.send(()).unwrap();
        assert_eq!(order_rx.recv_timeout(LONG_WAIT).unwrap(), "reader");
    });
}

#[test]
fn unrelated_waiter_can_bypass_blocked_waiter() {
    let coordinator = Arc::new(OperationCoordinator::new());
    let held = coordinator.acquire(OperationRequest::new().descriptor_exclusive(7));

    std::thread::scope(|scope| {
        let (blocked_done, blocked_rx) = mpsc::sync_channel(1);
        let blocked_worker = Arc::clone(&coordinator);
        scope.spawn(move || {
            let _lease = blocked_worker.acquire(OperationRequest::new().descriptor_exclusive(7));
            blocked_done.send(()).unwrap();
        });
        wait_until_waiting(&coordinator, 1);

        let (independent_done, independent_rx) = mpsc::sync_channel(1);
        let independent_worker = Arc::clone(&coordinator);
        scope.spawn(move || {
            let _lease =
                independent_worker.acquire(OperationRequest::new().descriptor_exclusive(8));
            independent_done.send(()).unwrap();
        });
        independent_rx.recv_timeout(LONG_WAIT).unwrap();
        assert!(matches!(
            blocked_rx.recv_timeout(SHORT_WAIT),
            Err(RecvTimeoutError::Timeout)
        ));

        drop(held);
        blocked_rx.recv_timeout(LONG_WAIT).unwrap();
    });
}

#[test]
fn multi_resource_request_is_acquired_atomically() {
    let coordinator = Arc::new(OperationCoordinator::new());
    let destination = coordinator.acquire(OperationRequest::new().descriptor_exclusive(8));

    std::thread::scope(|scope| {
        let (mixed_done, mixed_rx) = mpsc::sync_channel(1);
        let mixed_worker = Arc::clone(&coordinator);
        scope.spawn(move || {
            let _lease = mixed_worker.acquire(
                OperationRequest::new()
                    .descriptor_shared(7)
                    .descriptor_exclusive(8),
            );
            mixed_done.send(()).unwrap();
        });
        wait_until_waiting(&coordinator, 1);

        assert_eq!(coordinator.active_count_for_test(), 1);
        assert!(matches!(
            mixed_rx.recv_timeout(SHORT_WAIT),
            Err(RecvTimeoutError::Timeout)
        ));
        drop(destination);
        mixed_rx.recv_timeout(LONG_WAIT).unwrap();
    });
}

#[test]
fn unwinding_drops_lease_and_wakes_waiters() {
    let coordinator = OperationCoordinator::new();
    let result = catch_unwind(AssertUnwindSafe(|| {
        let _lease = coordinator.acquire(OperationRequest::new().mapping_registry_exclusive());
        panic!("release the coordinator lease");
    }));
    assert!(result.is_err());

    let _lease = coordinator.acquire(
        OperationRequest::new()
            .mapping_registry_shared()
            .address_shared(0x1000, 0x2000),
    );
}

#[test]
fn operation_coordinator_stress() {
    const THREADS: usize = 8;
    const ITERATIONS: usize = 500;

    let coordinator = Arc::new(OperationCoordinator::new());
    let start = Arc::new(Barrier::new(THREADS));
    let occupancy = AtomicIsize::new(0);

    std::thread::scope(|scope| {
        for thread in 0..THREADS {
            let coordinator = Arc::clone(&coordinator);
            let start = Arc::clone(&start);
            let occupancy = &occupancy;
            scope.spawn(move || {
                start.wait();
                for iteration in 0..ITERATIONS {
                    if (thread + iteration) % 4 == 0 {
                        let _lease = coordinator.acquire(
                            OperationRequest::new()
                                .descriptor_exclusive(7)
                                .address_exclusive(0x1000, 0x2000),
                        );
                        assert_eq!(
                            occupancy.compare_exchange(0, -1, Ordering::AcqRel, Ordering::Acquire,),
                            Ok(0)
                        );
                        std::thread::yield_now();
                        assert_eq!(occupancy.swap(0, Ordering::AcqRel), -1);
                    } else {
                        let _lease = coordinator.acquire(
                            OperationRequest::new()
                                .descriptor_shared(7)
                                .address_shared(0x1000, 0x2000),
                        );
                        let previous = occupancy.fetch_add(1, Ordering::AcqRel);
                        assert!(previous >= 0);
                        std::thread::yield_now();
                        assert!(occupancy.fetch_sub(1, Ordering::AcqRel) > 0);
                    }
                }
            });
        }
    });

    assert_eq!(occupancy.load(Ordering::Acquire), 0);
}
