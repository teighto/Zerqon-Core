use super::*;
use crate::{
    lifecycle::Lifecycle,
    player_control::{
        self, ControlBudget, ControlError, ControlOutcome, MESSAGE_RESERVATION_BYTES, SessionKey,
    },
};
use std::{
    future::Future,
    io,
    pin::Pin,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
};
use tokio::io::{AsyncReadExt, DuplexStream};

#[derive(Clone, Copy, Debug)]
struct ClosedTransport {
    reserved_bytes: usize,
    pending_jobs: usize,
}

#[derive(Default)]
struct Probe {
    written: AtomicUsize,
    closed: Mutex<Option<ClosedTransport>>,
}

struct ObservedDuplex {
    io: Option<DuplexStream>,
    probe: Arc<Probe>,
    budget: ControlBudget,
    lifecycle: Lifecycle,
    fail_flush: bool,
}

impl AsyncWrite for ObservedDuplex {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let result = Pin::new(self.io.as_mut().unwrap()).poll_write(context, bytes);
        if let Poll::Ready(Ok(written)) = result {
            self.probe.written.fetch_add(written, Ordering::Release);
        }
        result
    }

    fn poll_flush(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.fail_flush {
            Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "controlled flush failure",
            )))
        } else {
            Pin::new(self.io.as_mut().unwrap()).poll_flush(context)
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(self.io.as_mut().unwrap()).poll_shutdown(context)
    }
}

impl Drop for ObservedDuplex {
    fn drop(&mut self) {
        drop(self.io.take());
        *self.probe.closed.lock().unwrap() = Some(ClosedTransport {
            reserved_bytes: self.budget.snapshot().reserved_bytes,
            pending_jobs: self.lifecycle.pending().len(),
        });
    }
}

struct Harness {
    writer: Writer,
    peer: DuplexStream,
    sender: player_control::ControlSender,
    receiver: player_control::ControlReceiver,
    target: SessionKey,
    lifecycle: Lifecycle,
    budget: ControlBudget,
    shutdown: watch::Sender<bool>,
    probe: Arc<Probe>,
}

impl Harness {
    fn new(capacity: usize, deadline: Duration, fail_flush: bool) -> Self {
        let lifecycle = Lifecycle::new();
        let budget = ControlBudget::new(MESSAGE_RESERVATION_BYTES).unwrap();
        let target = SessionKey {
            uuid: Uuid::from_u128(42),
            session_id: 7,
        };
        let (sender, receiver, transport) =
            player_control::channel(target, budget.clone(), lifecycle.clone(), 2).unwrap();
        let (io, peer) = tokio::io::duplex(capacity);
        let (shutdown, stopped) = watch::channel(false);
        let probe = Arc::new(Probe::default());
        let writer = Writer {
            io: Box::new(ObservedDuplex {
                io: Some(io),
                probe: Arc::clone(&probe),
                budget: budget.clone(),
                lifecycle: lifecycle.clone(),
                fail_flush,
            }),
            compression: None,
            deadline,
            shutdown: stopped,
            _control_transport: Some(transport),
        };
        Self {
            writer,
            peer,
            sender,
            receiver,
            target,
            lifecycle,
            budget,
            shutdown,
            probe,
        }
    }
}

async fn pending<F: Future>(mut future: Pin<&mut F>) {
    std::future::poll_fn(|context| {
        assert!(future.as_mut().poll(context).is_pending());
        Poll::Ready(())
    })
    .await;
}

fn assert_transport_closed_before_receipt_release(probe: &Probe) {
    let closed = probe
        .closed
        .lock()
        .unwrap()
        .expect("Writer did not close its IO");
    assert_eq!(
        closed.reserved_bytes, MESSAGE_RESERVATION_BYTES,
        "control quota was released before IO closed"
    );
    assert_eq!(
        closed.pending_jobs, 1,
        "control lifecycle job completed before IO closed"
    );
}

#[tokio::test]
async fn queued_control_cancellation_never_writes_a_frame() {
    let mut h = Harness::new(1, Duration::from_secs(2), false);
    let stop = h.lifecycle.token().child();
    let receipt = h
        .sender
        .try_system_message(
            "cancelled before write",
            Instant::now() + Duration::from_secs(2),
            &stop,
        )
        .unwrap();
    let queued = h.receiver.recv().await.unwrap();
    stop.request_stop();
    h.writer.send_control(queued, h.target).await.unwrap();
    assert!(
        matches!(receipt.wait().await, ControlOutcome::Rejected { target, reason: ControlError::Cancelled, .. } if target == h.target)
    );
    assert_eq!(h.probe.written.load(Ordering::Acquire), 0);
    drop(h.writer);
    let mut bytes = Vec::new();
    timeout(Duration::from_secs(1), h.peer.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    assert!(bytes.is_empty());
    assert_eq!(h.budget.snapshot().reserved_bytes, 0);
    h.lifecycle
        .drain(Instant::now() + Duration::from_secs(1))
        .await
        .unwrap();
}

#[tokio::test]
async fn partial_control_write_failure_keeps_resources_until_writer_transport_closes() {
    for shutdown in [false, true] {
        let mut h = Harness::new(1, Duration::from_millis(30), false);
        let stop = h.lifecycle.token().child();
        let receipt = h
            .sender
            .try_system_message(
                "partial frame must remain owned",
                Instant::now() + Duration::from_secs(2),
                &stop,
            )
            .unwrap();
        let queued = h.receiver.recv().await.unwrap();
        let mut write = Box::pin(h.writer.send_control(queued, h.target));
        pending(write.as_mut()).await;
        assert_eq!(h.probe.written.load(Ordering::Acquire), 1);
        stop.request_stop();
        let mut result = Box::pin(receipt.wait());
        pending(result.as_mut()).await;
        if shutdown {
            h.shutdown.send_replace(true);
        }
        assert!(
            timeout(Duration::from_secs(1), write)
                .await
                .unwrap()
                .is_err()
        );
        pending(result.as_mut()).await;
        assert_eq!(
            h.budget.snapshot().reserved_bytes,
            MESSAGE_RESERVATION_BYTES
        );
        assert!(h.probe.closed.lock().unwrap().is_none());
        assert!(matches!(
            h.sender.try_system_message(
                "after failed frame",
                Instant::now() + Duration::from_secs(1),
                &h.lifecycle.token()
            ),
            Err(ControlError::Closed)
        ));
        let drain = h
            .lifecycle
            .drain(Instant::now() + Duration::from_millis(15))
            .await;
        assert!(matches!(
            drain,
            Err(crate::lifecycle::DrainError::Deadline { .. })
        ));
        drop(h.writer);
        assert_transport_closed_before_receipt_release(&h.probe);
        assert!(
            matches!(timeout(Duration::from_secs(1), result).await.unwrap(), ControlOutcome::ConnectionClosed { target, may_have_written: true, .. } if target == h.target)
        );
        let mut bytes = Vec::new();
        timeout(Duration::from_secs(1), h.peer.read_to_end(&mut bytes))
            .await
            .unwrap()
            .unwrap();
        assert!(!bytes.is_empty());
        assert!(
            read_frame(&mut bytes.as_slice(), 65_536, None)
                .await
                .is_err()
        );
        assert_eq!(h.budget.snapshot().reserved_bytes, 0);
        h.sender.drain().await;
        h.lifecycle
            .drain(Instant::now() + Duration::from_secs(1))
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn dropping_partial_control_send_does_not_release_the_live_transport() {
    let mut h = Harness::new(1, Duration::from_secs(2), false);
    let receipt = h
        .sender
        .try_system_message(
            "abandoned send",
            Instant::now() + Duration::from_secs(2),
            &h.lifecycle.token(),
        )
        .unwrap();
    let queued = h.receiver.recv().await.unwrap();
    let mut write = Box::pin(h.writer.send_control(queued, h.target));
    pending(write.as_mut()).await;
    assert_eq!(h.probe.written.load(Ordering::Acquire), 1);
    drop(write);
    h.receiver.close();
    let mut result = Box::pin(receipt.wait());
    pending(result.as_mut()).await;
    assert_eq!(
        h.budget.snapshot().reserved_bytes,
        MESSAGE_RESERVATION_BYTES
    );
    let mut drain = Box::pin(h.sender.drain());
    pending(drain.as_mut()).await;
    drop(drain);
    assert!(h.probe.closed.lock().unwrap().is_none());
    drop(h.writer);
    assert_transport_closed_before_receipt_release(&h.probe);
    assert!(matches!(
        result.await,
        ControlOutcome::ConnectionClosed {
            may_have_written: true,
            ..
        }
    ));
    h.sender.drain().await;
    h.lifecycle
        .drain(Instant::now() + Duration::from_secs(1))
        .await
        .unwrap();
}

#[tokio::test]
async fn cancellation_after_partial_write_still_reports_a_completed_frame_as_written() {
    let mut h = Harness::new(1, Duration::from_secs(2), false);
    let stop = h.lifecycle.token().child();
    let receipt = h
        .sender
        .try_system_message(
            "completed after cancellation",
            Instant::now() + Duration::from_secs(2),
            &stop,
        )
        .unwrap();
    let queued = h.receiver.recv().await.unwrap();
    let mut write = Box::pin(h.writer.send_control(queued, h.target));
    pending(write.as_mut()).await;
    assert_eq!(h.probe.written.load(Ordering::Acquire), 1);
    stop.request_stop();
    let mut result = Box::pin(receipt.wait());
    pending(result.as_mut()).await;
    let (written, packet) = timeout(Duration::from_secs(1), async {
        tokio::join!(write, read_frame(&mut h.peer, 65_536, None))
    })
    .await
    .unwrap();
    written.unwrap();
    let packet = packet.unwrap();
    assert_eq!(packet[0], 124);
    assert!(String::from_utf8_lossy(&packet).contains("completed after cancellation"));
    assert!(matches!(result.await, ControlOutcome::Written { target, .. } if target == h.target));
    assert_eq!(h.budget.snapshot().reserved_bytes, 0);
    assert!(h.lifecycle.pending().is_empty());
    assert!(h.probe.closed.lock().unwrap().is_none());
}

#[tokio::test]
async fn a_complete_frame_with_failed_flush_never_reports_written() {
    let mut h = Harness::new(4096, Duration::from_secs(2), true);
    let receipt = h
        .sender
        .try_system_message(
            "flush failed",
            Instant::now() + Duration::from_secs(2),
            &h.lifecycle.token(),
        )
        .unwrap();
    let queued = h.receiver.recv().await.unwrap();
    assert!(h.writer.send_control(queued, h.target).await.is_err());
    let packet = read_frame(&mut h.peer, 65_536, None).await.unwrap();
    assert_eq!(packet[0], 124);
    assert!(String::from_utf8_lossy(&packet).contains("flush failed"));
    let mut result = Box::pin(receipt.wait());
    pending(result.as_mut()).await;
    assert_eq!(
        h.budget.snapshot().reserved_bytes,
        MESSAGE_RESERVATION_BYTES
    );
    drop(h.writer);
    assert_transport_closed_before_receipt_release(&h.probe);
    assert!(matches!(
        result.await,
        ControlOutcome::ConnectionClosed {
            may_have_written: true,
            ..
        }
    ));
    h.lifecycle
        .drain(Instant::now() + Duration::from_secs(1))
        .await
        .unwrap();
}

#[tokio::test]
async fn closing_writer_before_releasing_write_owner_keeps_quota_until_both_are_done() {
    let mut h = Harness::new(1, Duration::from_secs(2), false);
    let receipt = h
        .sender
        .try_system_message(
            "owner survives writer",
            Instant::now() + Duration::from_secs(2),
            &h.lifecycle.token(),
        )
        .unwrap();
    let queued = h.receiver.recv().await.unwrap();
    let writing = queued.begin_write(h.target).unwrap();
    let mut send = Box::pin(h.writer.send(writing.packet()));
    pending(send.as_mut()).await;
    assert_eq!(h.probe.written.load(Ordering::Acquire), 1);
    drop(send);
    drop(h.writer);
    assert_transport_closed_before_receipt_release(&h.probe);
    let mut result = Box::pin(receipt.wait());
    pending(result.as_mut()).await;
    assert_eq!(
        h.budget.snapshot().reserved_bytes,
        MESSAGE_RESERVATION_BYTES
    );
    writing.finish_closed();
    assert!(matches!(
        result.await,
        ControlOutcome::ConnectionClosed {
            may_have_written: true,
            ..
        }
    ));
    assert_eq!(h.budget.snapshot().reserved_bytes, 0);
    h.lifecycle
        .drain(Instant::now() + Duration::from_secs(1))
        .await
        .unwrap();
}
