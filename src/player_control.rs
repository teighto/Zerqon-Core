use crate::{
    lifecycle::{JobGuard, Lifecycle, RegistrationError, StopToken},
    packets,
};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
};
use tokio::{
    sync::{Notify, OwnedSemaphorePermit, Semaphore, mpsc},
    time::Instant,
};
use uuid::Uuid;

pub const MAX_MESSAGE_BYTES: usize = 4096;
pub const MESSAGE_RESERVATION_BYTES: usize = 64 * 1024;
pub const DEFAULT_BUDGET_BYTES: usize = 4 * 1024 * 1024;
const MAX_QUEUE_CAPACITY: usize = 256;
static NEXT_OPERATION: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionKey {
    pub uuid: Uuid,
    pub session_id: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ControlError {
    #[error("invalid player control target")]
    InvalidTarget,
    #[error("invalid player control limits")]
    InvalidLimits,
    #[error("system message exceeds 4096 UTF-8 bytes")]
    TooLarge,
    #[error("player control queue or byte budget is full")]
    Busy,
    #[error("player control is closed")]
    Closed,
    #[error("player control request cancelled before writing")]
    Cancelled,
    #[error("player control deadline expired before writing")]
    Deadline,
    #[error("player control target is no longer the current session")]
    StaleTarget,
    #[error("player control operation identifiers exhausted")]
    IdentifierExhausted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlOutcome {
    Written {
        target: SessionKey,
        operation_id: u64,
    },
    Rejected {
        target: SessionKey,
        operation_id: u64,
        reason: ControlError,
    },
    ConnectionClosed {
        target: SessionKey,
        operation_id: u64,
        may_have_written: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BudgetSnapshot {
    pub limit_bytes: usize,
    pub reserved_bytes: usize,
    pub peak_bytes: usize,
    pub rejections: u64,
}

#[derive(Clone)]
pub struct ControlBudget(Arc<Mutex<BudgetSnapshot>>);

impl Default for ControlBudget {
    fn default() -> Self {
        Self::new(DEFAULT_BUDGET_BYTES).expect("default control budget is valid")
    }
}

impl ControlBudget {
    pub fn new(limit_bytes: usize) -> Result<Self, ControlError> {
        if !(MESSAGE_RESERVATION_BYTES..=isize::MAX as usize).contains(&limit_bytes) {
            return Err(ControlError::InvalidLimits);
        }
        Ok(Self(Arc::new(Mutex::new(BudgetSnapshot {
            limit_bytes,
            reserved_bytes: 0,
            peak_bytes: 0,
            rejections: 0,
        }))))
    }

    pub fn snapshot(&self) -> BudgetSnapshot {
        *self.0.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn reserve(&self) -> Result<ByteLease, ControlError> {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        if state.limit_bytes - state.reserved_bytes < MESSAGE_RESERVATION_BYTES {
            state.rejections = state.rejections.saturating_add(1);
            return Err(ControlError::Busy);
        }
        state.reserved_bytes += MESSAGE_RESERVATION_BYTES;
        state.peak_bytes = state.peak_bytes.max(state.reserved_bytes);
        Ok(ByteLease(self.clone()))
    }
}

struct ByteLease(ControlBudget);

impl Drop for ByteLease {
    fn drop(&mut self) {
        let mut state = self.0.0.lock().unwrap_or_else(|error| error.into_inner());
        state.reserved_bytes -= MESSAGE_RESERVATION_BYTES;
    }
}

#[derive(Default)]
struct Activity {
    count: AtomicUsize,
    changed: Notify,
}

struct ActivityLease {
    activity: Arc<Activity>,
    permit: Option<OwnedSemaphorePermit>,
}

impl ActivityLease {
    fn new(activity: Arc<Activity>, permit: OwnedSemaphorePermit) -> Self {
        activity.count.fetch_add(1, Ordering::AcqRel);
        Self {
            activity,
            permit: Some(permit),
        }
    }

    fn release_permit(&mut self) {
        drop(self.permit.take());
    }
}

impl Drop for ActivityLease {
    fn drop(&mut self) {
        self.release_permit();
        self.activity.count.fetch_sub(1, Ordering::AcqRel);
        self.activity.changed.notify_waiters();
    }
}

struct MessageData {
    packet: Option<Box<[u8]>>,
    bytes: Option<ByteLease>,
    activity: Option<ActivityLease>,
    job: Option<JobGuard>,
}

impl Drop for MessageData {
    fn drop(&mut self) {
        drop(self.packet.take());
        drop(self.bytes.take());
        if let Some(activity) = &mut self.activity {
            activity.release_permit();
        }
        if let Some(job) = self.job.take() {
            job.finish(Ok(()));
        }
        drop(self.activity.take());
    }
}

struct Gate {
    closed: bool,
    requests: BTreeMap<u64, Arc<Request>>,
}

struct Channel {
    target: SessionKey,
    budget: ControlBudget,
    lifecycle: Lifecycle,
    slots: Arc<Semaphore>,
    activity: Arc<Activity>,
    gate: Mutex<Gate>,
    transport_closed: AtomicBool,
}

impl Channel {
    fn close(&self, transport_closed: bool) {
        let requests = {
            let mut gate = self.gate.lock().unwrap_or_else(|error| error.into_inner());
            gate.closed = true;
            if transport_closed {
                self.transport_closed.store(true, Ordering::Release);
            }
            gate.requests.values().cloned().collect::<Vec<_>>()
        };
        for request in requests {
            request.reject_queued(ControlError::Closed);
            if transport_closed {
                request.settle_closed();
            }
        }
    }
}

#[derive(Clone)]
pub struct ControlSender {
    channel: Arc<Channel>,
    sender: mpsc::Sender<QueuedMessage>,
}

pub struct ControlReceiver {
    channel: Arc<Channel>,
    receiver: mpsc::Receiver<QueuedMessage>,
}

/// Must be owned by the sole Writer and dropped after its IO transport.
pub struct TransportGuard {
    channel: Arc<Channel>,
}

pub fn channel(
    target: SessionKey,
    budget: ControlBudget,
    lifecycle: Lifecycle,
    capacity: usize,
) -> Result<(ControlSender, ControlReceiver, TransportGuard), ControlError> {
    if target.uuid.is_nil() || target.session_id <= 0 {
        return Err(ControlError::InvalidTarget);
    }
    if !(1..=MAX_QUEUE_CAPACITY).contains(&capacity) {
        return Err(ControlError::InvalidLimits);
    }
    let (sender, receiver) = mpsc::channel(capacity);
    let channel = Arc::new(Channel {
        target,
        budget,
        lifecycle,
        slots: Arc::new(Semaphore::new(capacity)),
        activity: Arc::new(Activity::default()),
        gate: Mutex::new(Gate {
            closed: false,
            requests: BTreeMap::new(),
        }),
        transport_closed: AtomicBool::new(false),
    });
    Ok((
        ControlSender {
            channel: Arc::clone(&channel),
            sender,
        },
        ControlReceiver {
            channel: Arc::clone(&channel),
            receiver,
        },
        TransportGuard { channel },
    ))
}

impl ControlSender {
    pub fn target(&self) -> SessionKey {
        self.channel.target
    }

    pub fn try_system_message(
        &self,
        text: &str,
        deadline: Instant,
        stop: &StopToken,
    ) -> Result<ControlReceipt, ControlError> {
        if text.len() > MAX_MESSAGE_BYTES {
            return Err(ControlError::TooLarge);
        }
        check_request(deadline, stop)?;
        if self
            .channel
            .gate
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .closed
        {
            return Err(ControlError::Closed);
        }
        let queue = self
            .sender
            .clone()
            .try_reserve_owned()
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => ControlError::Busy,
                mpsc::error::TrySendError::Closed(_) => ControlError::Closed,
            })?;
        let slot = Arc::clone(&self.channel.slots)
            .try_acquire_owned()
            .map_err(|_| ControlError::Busy)?;
        let activity = {
            let gate = self
                .channel
                .gate
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if gate.closed {
                return Err(ControlError::Closed);
            }
            ActivityLease::new(Arc::clone(&self.channel.activity), slot)
        };
        let bytes = self.channel.budget.reserve()?;
        let job = self
            .channel
            .lifecycle
            .register("player control message")
            .map_err(|error| match error {
                RegistrationError::Capacity => ControlError::Busy,
                RegistrationError::Stopped | RegistrationError::Sealed => ControlError::Closed,
            })?;
        let mut data = MessageData {
            packet: None,
            bytes: Some(bytes),
            activity: Some(activity),
            job: Some(job),
        };
        let id = next_operation(&NEXT_OPERATION)?;
        data.packet = Some(packets::system_chat(text).into_boxed_slice());
        check_request(deadline, stop)?;
        let request = Arc::new(Request {
            channel: Arc::downgrade(&self.channel),
            target: self.channel.target,
            id,
            deadline,
            stop: stop.child(),
            state: Mutex::new(RequestState::Queued(Some(data))),
            changed: Notify::new(),
        });
        {
            let mut gate = self
                .channel
                .gate
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if gate.closed {
                return Err(ControlError::Closed);
            }
            gate.requests.insert(id, Arc::clone(&request));
        }
        queue.send(QueuedMessage {
            request: Arc::clone(&request),
        });
        Ok(ControlReceipt { request })
    }

    /// Wait for accepted resources after the receiver has closed admission.
    pub async fn drain(&self) {
        loop {
            let changed = self.channel.activity.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.channel.activity.count.load(Ordering::Acquire) == 0 {
                return;
            }
            changed.await;
        }
    }
}

impl ControlReceiver {
    pub async fn recv(&mut self) -> Option<QueuedMessage> {
        self.receiver.recv().await
    }

    pub fn close(&mut self) {
        self.channel.close(false);
        self.receiver.close();
        while self.receiver.try_recv().is_ok() {}
    }
}

impl Drop for ControlReceiver {
    fn drop(&mut self) {
        self.close();
    }
}

impl Drop for TransportGuard {
    fn drop(&mut self) {
        self.channel.close(true);
    }
}

fn check_request(deadline: Instant, stop: &StopToken) -> Result<(), ControlError> {
    if stop.is_stopped() {
        Err(ControlError::Cancelled)
    } else if Instant::now() >= deadline {
        Err(ControlError::Deadline)
    } else {
        Ok(())
    }
}

fn next_operation(counter: &AtomicU64) -> Result<u64, ControlError> {
    counter
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
        .map_err(|_| ControlError::IdentifierExhausted)
}

enum RequestState {
    Queued(Option<MessageData>),
    Writing,
    AwaitingTransportClose(Option<MessageData>),
    Settled(ControlOutcome),
}

struct Request {
    channel: Weak<Channel>,
    target: SessionKey,
    id: u64,
    deadline: Instant,
    stop: StopToken,
    state: Mutex<RequestState>,
    changed: Notify,
}

impl Request {
    fn rejection(&self, reason: ControlError) -> ControlOutcome {
        ControlOutcome::Rejected {
            target: self.target,
            operation_id: self.id,
            reason,
        }
    }

    fn retire(&self) {
        if let Some(channel) = self.channel.upgrade() {
            channel
                .gate
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .requests
                .remove(&self.id);
        }
        self.changed.notify_waiters();
    }

    fn reject_queued(&self, reason: ControlError) {
        {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            let RequestState::Queued(data) = &mut *state else {
                return;
            };
            drop(data.take());
            *state = RequestState::Settled(self.rejection(reason));
        }
        self.retire();
    }

    fn transport_is_closed(&self) -> bool {
        self.channel
            .upgrade()
            .is_none_or(|channel| channel.transport_closed.load(Ordering::Acquire))
    }

    fn settle_closed(&self) {
        if !self.transport_is_closed() {
            return;
        }
        {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            let RequestState::AwaitingTransportClose(data) = &mut *state else {
                return;
            };
            drop(data.take());
            *state = RequestState::Settled(ControlOutcome::ConnectionClosed {
                target: self.target,
                operation_id: self.id,
                may_have_written: true,
            });
        }
        self.retire();
    }

    fn finish_written(&self, data: MessageData) {
        {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            assert!(
                matches!(*state, RequestState::Writing),
                "write owner settles once"
            );
            drop(data);
            *state = RequestState::Settled(ControlOutcome::Written {
                target: self.target,
                operation_id: self.id,
            });
        }
        self.retire();
    }

    fn await_transport_close(&self, data: MessageData) {
        {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            assert!(
                matches!(*state, RequestState::Writing),
                "write owner settles once"
            );
            *state = RequestState::AwaitingTransportClose(Some(data));
        }
        if let Some(channel) = self.channel.upgrade() {
            channel.close(false);
        }
        self.settle_closed();
    }
}

pub struct QueuedMessage {
    request: Arc<Request>,
}

impl QueuedMessage {
    pub fn begin_write(self, owner_key: SessionKey) -> Option<WritingMessage> {
        let data = {
            let mut state = self
                .request
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let RequestState::Queued(data) = &mut *state else {
                return None;
            };
            let channel = self.request.channel.upgrade();
            let gate = channel.as_ref().map(|channel| {
                channel
                    .gate
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
            });
            let error = if owner_key != self.request.target {
                Some(ControlError::StaleTarget)
            } else if gate.as_ref().is_none_or(|gate| gate.closed) {
                Some(ControlError::Closed)
            } else {
                check_request(self.request.deadline, &self.request.stop).err()
            };
            if let Some(error) = error {
                drop(data.take());
                *state = RequestState::Settled(self.request.rejection(error));
                drop(gate);
                drop(state);
                self.request.retire();
                return None;
            }
            let data = data.take().expect("queued request owns its data");
            *state = RequestState::Writing;
            data
        };
        Some(WritingMessage {
            request: Arc::clone(&self.request),
            data: Some(data),
        })
    }
}

impl Drop for QueuedMessage {
    fn drop(&mut self) {
        self.request.reject_queued(ControlError::Cancelled);
    }
}

pub struct WritingMessage {
    request: Arc<Request>,
    data: Option<MessageData>,
}

impl WritingMessage {
    pub fn packet(&self) -> &[u8] {
        self.data
            .as_ref()
            .and_then(|data| data.packet.as_deref())
            .expect("writing request owns its packet")
    }

    pub fn finish_written(mut self) {
        self.request
            .finish_written(self.data.take().expect("write owner settles once"));
    }

    pub fn finish_closed(self) {
        drop(self);
    }
}

impl Drop for WritingMessage {
    fn drop(&mut self) {
        if let Some(data) = self.data.take() {
            self.request.await_transport_close(data);
        }
    }
}

pub struct ControlReceipt {
    request: Arc<Request>,
}

impl ControlReceipt {
    pub async fn wait(self) -> ControlOutcome {
        loop {
            let changed = self.request.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let queued = {
                let state = self
                    .request
                    .state
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                match *state {
                    RequestState::Settled(outcome) => return outcome,
                    RequestState::Queued(_) => true,
                    RequestState::Writing | RequestState::AwaitingTransportClose(_) => false,
                }
            };
            if queued {
                tokio::select! {
                    biased;
                    _ = changed => {},
                    _ = self.request.stop.cancelled() => self.request.reject_queued(ControlError::Cancelled),
                    _ = tokio::time::sleep_until(self.request.deadline) => self.request.reject_queued(ControlError::Deadline),
                }
            } else {
                changed.await;
            }
        }
    }
}

impl Drop for ControlReceipt {
    fn drop(&mut self) {
        self.request.reject_queued(ControlError::Cancelled);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{future::Future, pin::Pin, task::Poll, time::Duration};

    fn key(id: i32) -> SessionKey {
        SessionKey {
            uuid: Uuid::from_u128(1),
            session_id: id,
        }
    }

    fn deadline() -> Instant {
        Instant::now() + Duration::from_secs(2)
    }

    async fn pending<F: Future>(mut future: Pin<&mut F>) {
        std::future::poll_fn(|context| {
            assert!(future.as_mut().poll(context).is_pending());
            Poll::Ready(())
        })
        .await;
    }

    async fn done<F: Future>(future: F) -> F::Output {
        tokio::time::timeout(Duration::from_secs(1), future)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn global_bytes_are_shared_and_cancelled_queue_entries_release_resources() {
        let lifecycle = Lifecycle::new();
        let budget = ControlBudget::new(MESSAGE_RESERVATION_BYTES).unwrap();
        let (first, mut inbox, _first_transport) =
            channel(key(1), budget.clone(), lifecycle.clone(), 2).unwrap();
        let (second, _other_inbox, _second_transport) =
            channel(key(2), budget.clone(), lifecycle.clone(), 2).unwrap();
        let stop = lifecycle.token();
        let receipt = first
            .try_system_message("first", deadline(), &stop)
            .unwrap();
        assert!(matches!(
            second.try_system_message("second", deadline(), &stop),
            Err(ControlError::Busy)
        ));
        assert_eq!(budget.snapshot().reserved_bytes, MESSAGE_RESERVATION_BYTES);
        drop(receipt);
        assert_eq!(budget.snapshot().reserved_bytes, 0);
        assert!(
            done(inbox.recv())
                .await
                .unwrap()
                .begin_write(key(1))
                .is_none()
        );
        let next = second
            .try_system_message("second", deadline(), &stop)
            .unwrap();
        drop(next);
        assert_eq!(budget.snapshot().reserved_bytes, 0);
        assert!(lifecycle.pending().is_empty());
    }

    #[tokio::test]
    async fn outstanding_limit_counts_writing_requests_after_dequeue() {
        let lifecycle = Lifecycle::new();
        let budget = ControlBudget::new(3 * MESSAGE_RESERVATION_BYTES).unwrap();
        let (sender, mut inbox, _transport) =
            channel(key(1), budget.clone(), lifecycle.clone(), 2).unwrap();
        let stop = lifecycle.token();
        let first = sender
            .try_system_message("first", deadline(), &stop)
            .unwrap();
        let writing = done(inbox.recv())
            .await
            .unwrap()
            .begin_write(key(1))
            .unwrap();
        let queued = sender
            .try_system_message("queued", deadline(), &stop)
            .unwrap();
        assert!(matches!(
            sender.try_system_message("excess", deadline(), &stop),
            Err(ControlError::Busy)
        ));
        assert_eq!(
            budget.snapshot().reserved_bytes,
            2 * MESSAGE_RESERVATION_BYTES
        );
        writing.finish_written();
        assert!(
            matches!(done(first.wait()).await, ControlOutcome::Written { target, .. } if target == key(1))
        );
        drop(queued);
        inbox.close();
        done(sender.drain()).await;
        assert_eq!(budget.snapshot().reserved_bytes, 0);
        assert!(lifecycle.pending().is_empty());
    }

    #[tokio::test]
    async fn oversized_input_is_rejected_before_quota_and_encoding() {
        let lifecycle = Lifecycle::new();
        let budget = ControlBudget::default();
        let (sender, _inbox, _transport) =
            channel(key(1), budget.clone(), lifecycle.clone(), 1).unwrap();
        assert!(matches!(
            sender.try_system_message(
                &"x".repeat(MAX_MESSAGE_BYTES + 1),
                deadline(),
                &lifecycle.token()
            ),
            Err(ControlError::TooLarge)
        ));
        assert_eq!(budget.snapshot().peak_bytes, 0);
        assert!(lifecycle.pending().is_empty());
    }

    #[tokio::test]
    async fn system_message_preserves_nul_and_non_bmp_text_in_the_host_packet() {
        let lifecycle = Lifecycle::new();
        let (sender, mut inbox, _transport) =
            channel(key(1), ControlBudget::default(), lifecycle.clone(), 1).unwrap();
        let receipt = sender
            .try_system_message("\0😀", deadline(), &lifecycle.token())
            .unwrap();
        let writing = done(inbox.recv())
            .await
            .unwrap()
            .begin_write(key(1))
            .unwrap();
        assert_eq!(
            writing.packet(),
            &[
                124, 8, 0, 8, 0xc0, 0x80, 0xed, 0xa0, 0xbd, 0xed, 0xb8, 0x80, 0
            ]
        );
        writing.finish_written();
        assert!(matches!(
            done(receipt.wait()).await,
            ControlOutcome::Written { .. }
        ));
        let receipt = sender
            .try_system_message(
                &"\0".repeat(MAX_MESSAGE_BYTES),
                deadline(),
                &lifecycle.token(),
            )
            .unwrap();
        let writing = done(inbox.recv())
            .await
            .unwrap()
            .begin_write(key(1))
            .unwrap();
        assert_eq!(writing.packet().len(), 8197);
        assert_eq!(&writing.packet()[..4], &[124, 8, 32, 0]);
        writing.finish_written();
        assert!(matches!(
            done(receipt.wait()).await,
            ControlOutcome::Written { .. }
        ));
    }

    #[tokio::test]
    async fn queued_cancellation_and_deadline_reject_without_a_write_owner() {
        let lifecycle = Lifecycle::new();
        let budget = ControlBudget::default();
        let (sender, mut inbox, _transport) =
            channel(key(1), budget.clone(), lifecycle.clone(), 2).unwrap();
        let stop = lifecycle.token().child();
        let receipt = sender
            .try_system_message("cancel", deadline(), &stop)
            .unwrap();
        stop.request_stop();
        assert!(matches!(
            done(receipt.wait()).await,
            ControlOutcome::Rejected {
                reason: ControlError::Cancelled,
                ..
            }
        ));
        assert!(
            done(inbox.recv())
                .await
                .unwrap()
                .begin_write(key(1))
                .is_none()
        );
        let expires = Instant::now() + Duration::from_millis(5);
        let receipt = sender
            .try_system_message("deadline", expires, &lifecycle.token())
            .unwrap();
        tokio::time::sleep_until(expires).await;
        assert!(matches!(
            done(receipt.wait()).await,
            ControlOutcome::Rejected {
                reason: ControlError::Deadline,
                ..
            }
        ));
        assert!(
            done(inbox.recv())
                .await
                .unwrap()
                .begin_write(key(1))
                .is_none()
        );
        assert_eq!(budget.snapshot().reserved_bytes, 0);
        assert!(lifecycle.pending().is_empty());
    }

    #[tokio::test]
    async fn begun_write_keeps_resources_and_settles_success_after_caller_cancel() {
        let lifecycle = Lifecycle::new();
        let budget = ControlBudget::default();
        let (sender, mut inbox, _transport) =
            channel(key(1), budget.clone(), lifecycle.clone(), 1).unwrap();
        let stop = lifecycle.token().child();
        let receipt = sender
            .try_system_message("started", deadline(), &stop)
            .unwrap();
        let writing = done(inbox.recv())
            .await
            .unwrap()
            .begin_write(key(1))
            .unwrap();
        stop.request_stop();
        let mut waiting = Box::pin(receipt.wait());
        pending(waiting.as_mut()).await;
        assert_eq!(budget.snapshot().reserved_bytes, MESSAGE_RESERVATION_BYTES);
        assert!(!lifecycle.pending().is_empty());
        writing.finish_written();
        assert_eq!(budget.snapshot().reserved_bytes, 0);
        assert!(lifecycle.pending().is_empty());
        assert!(matches!(
            done(waiting).await,
            ControlOutcome::Written { .. }
        ));
    }

    #[tokio::test]
    async fn abandoned_write_waits_for_transport_close_after_receiver_and_caller_disappear() {
        let lifecycle = Lifecycle::new();
        let budget = ControlBudget::default();
        let (sender, mut inbox, transport) =
            channel(key(1), budget.clone(), lifecycle.clone(), 1).unwrap();
        let receipt = sender
            .try_system_message("owned", deadline(), &lifecycle.token())
            .unwrap();
        let writing = done(inbox.recv())
            .await
            .unwrap()
            .begin_write(key(1))
            .unwrap();
        drop(receipt);
        drop(writing);
        drop(inbox);
        assert_eq!(budget.snapshot().reserved_bytes, MESSAGE_RESERVATION_BYTES);
        let mut drained = Box::pin(sender.drain());
        pending(drained.as_mut()).await;
        lifecycle.stop();
        let pending_cleanup = lifecycle
            .drain(Instant::now() + Duration::from_millis(5))
            .await;
        assert!(matches!(
            pending_cleanup,
            Err(crate::lifecycle::DrainError::Deadline { .. })
        ));
        drop(transport);
        done(drained).await;
        assert_eq!(budget.snapshot().reserved_bytes, 0);
        done(lifecycle.drain(deadline())).await.unwrap();
    }

    #[tokio::test]
    async fn writing_close_receipt_waits_for_the_transport_fence_in_both_drop_orders() {
        for close_transport_first in [false, true] {
            let lifecycle = Lifecycle::new();
            let budget = ControlBudget::default();
            let (sender, mut inbox, transport) =
                channel(key(1), budget.clone(), lifecycle.clone(), 1).unwrap();
            let receipt = sender
                .try_system_message("partial", deadline(), &lifecycle.token())
                .unwrap();
            let writing = done(inbox.recv())
                .await
                .unwrap()
                .begin_write(key(1))
                .unwrap();
            let mut waiting = Box::pin(receipt.wait());
            if close_transport_first {
                drop(transport);
                pending(waiting.as_mut()).await;
                assert_eq!(budget.snapshot().reserved_bytes, MESSAGE_RESERVATION_BYTES);
                writing.finish_closed();
            } else {
                writing.finish_closed();
                pending(waiting.as_mut()).await;
                assert_eq!(budget.snapshot().reserved_bytes, MESSAGE_RESERVATION_BYTES);
                drop(transport);
            }
            assert_eq!(budget.snapshot().reserved_bytes, 0);
            assert!(lifecycle.pending().is_empty());
            assert!(
                matches!(done(waiting).await, ControlOutcome::ConnectionClosed { target, may_have_written: true, .. } if target == key(1))
            );
        }
    }

    #[tokio::test]
    async fn settled_written_wins_over_later_deadline_and_cancellation() {
        let lifecycle = Lifecycle::new();
        let (sender, mut inbox, transport) =
            channel(key(1), ControlBudget::default(), lifecycle.clone(), 1).unwrap();
        let stop = lifecycle.token().child();
        let expires = Instant::now() + Duration::from_millis(10);
        let receipt = sender
            .try_system_message("written", expires, &stop)
            .unwrap();
        done(inbox.recv())
            .await
            .unwrap()
            .begin_write(key(1))
            .unwrap()
            .finish_written();
        stop.request_stop();
        drop(transport);
        tokio::time::sleep_until(expires).await;
        assert!(matches!(
            done(receipt.wait()).await,
            ControlOutcome::Written { .. }
        ));
    }

    #[tokio::test]
    async fn stale_owner_is_rejected_and_old_sender_cannot_target_a_replacement() {
        let lifecycle = Lifecycle::new();
        let budget = ControlBudget::default();
        let (old, mut inbox, transport) =
            channel(key(1), budget.clone(), lifecycle.clone(), 1).unwrap();
        let receipt = old
            .try_system_message("stale", deadline(), &lifecycle.token())
            .unwrap();
        assert!(
            done(inbox.recv())
                .await
                .unwrap()
                .begin_write(key(2))
                .is_none()
        );
        assert!(matches!(
            done(receipt.wait()).await,
            ControlOutcome::Rejected {
                reason: ControlError::StaleTarget,
                ..
            }
        ));
        drop(inbox);
        drop(transport);
        let (new, mut new_inbox, _new_transport) =
            channel(key(2), budget.clone(), lifecycle.clone(), 1).unwrap();
        assert!(matches!(
            old.try_system_message("late", deadline(), &lifecycle.token()),
            Err(ControlError::Closed)
        ));
        let receipt = new
            .try_system_message("current", deadline(), &lifecycle.token())
            .unwrap();
        done(new_inbox.recv())
            .await
            .unwrap()
            .begin_write(key(2))
            .unwrap()
            .finish_written();
        assert!(
            matches!(done(receipt.wait()).await, ControlOutcome::Written { target, .. } if target == key(2))
        );
        assert_eq!(budget.snapshot().reserved_bytes, 0);
    }

    #[tokio::test]
    async fn closing_receiver_settles_unstarted_requests_and_releases_all_quotas() {
        let lifecycle = Lifecycle::new();
        let budget = ControlBudget::default();
        let (sender, inbox, _transport) =
            channel(key(1), budget.clone(), lifecycle.clone(), 1).unwrap();
        let receipt = sender
            .try_system_message("queued", deadline(), &lifecycle.token())
            .unwrap();
        drop(inbox);
        assert_eq!(budget.snapshot().reserved_bytes, 0);
        assert!(lifecycle.pending().is_empty());
        assert!(matches!(
            done(receipt.wait()).await,
            ControlOutcome::Rejected {
                reason: ControlError::Closed,
                ..
            }
        ));
        done(sender.drain()).await;
    }

    #[tokio::test]
    async fn closed_transport_rejects_new_messages_while_a_write_retains_the_only_slot() {
        let lifecycle = Lifecycle::new();
        let (sender, mut inbox, transport) =
            channel(key(1), ControlBudget::default(), lifecycle.clone(), 1).unwrap();
        let receipt = sender
            .try_system_message("started", deadline(), &lifecycle.token())
            .unwrap();
        let writing = done(inbox.recv())
            .await
            .unwrap()
            .begin_write(key(1))
            .unwrap();
        drop(transport);
        let retry = sender.try_system_message("late", deadline(), &lifecycle.token());
        writing.finish_closed();
        assert!(matches!(
            done(receipt.wait()).await,
            ControlOutcome::ConnectionClosed { .. }
        ));
        assert!(
            matches!(retry, Err(ControlError::Closed)),
            "closed target must not be reported as temporary capacity pressure"
        );
    }

    #[tokio::test]
    async fn abandoned_write_closes_admission_before_transport_cleanup_finishes() {
        let lifecycle = Lifecycle::new();
        let (sender, mut inbox, transport) =
            channel(key(1), ControlBudget::default(), lifecycle.clone(), 2).unwrap();
        let receipt = sender
            .try_system_message("failed", deadline(), &lifecycle.token())
            .unwrap();
        let writing = done(inbox.recv())
            .await
            .unwrap()
            .begin_write(key(1))
            .unwrap();
        writing.finish_closed();
        let retry = sender.try_system_message(
            "must not enter a failed writer",
            deadline(),
            &lifecycle.token(),
        );
        let refused = matches!(&retry, Err(ControlError::Closed));
        drop(retry);
        let mut waiting = Box::pin(receipt.wait());
        pending(waiting.as_mut()).await;
        drop(transport);
        assert!(matches!(
            done(waiting).await,
            ControlOutcome::ConnectionClosed { .. }
        ));
        assert!(
            refused,
            "failed writer continued admitting messages before transport close"
        );
    }

    #[test]
    fn invalid_targets_limits_and_exhausted_ids_cannot_enter_the_queue() {
        let lifecycle = Lifecycle::new();
        for target in [
            SessionKey {
                uuid: Uuid::nil(),
                session_id: 1,
            },
            key(0),
            key(-1),
        ] {
            assert!(matches!(
                channel(target, ControlBudget::default(), lifecycle.clone(), 1),
                Err(ControlError::InvalidTarget)
            ));
        }
        for capacity in [0, MAX_QUEUE_CAPACITY + 1] {
            assert!(matches!(
                channel(
                    key(1),
                    ControlBudget::default(),
                    lifecycle.clone(),
                    capacity
                ),
                Err(ControlError::InvalidLimits)
            ));
        }
        assert!(matches!(
            ControlBudget::new(MESSAGE_RESERVATION_BYTES - 1),
            Err(ControlError::InvalidLimits)
        ));
        let counter = AtomicU64::new(u64::MAX - 1);
        assert_eq!(next_operation(&counter), Ok(u64::MAX - 1));
        assert_eq!(
            next_operation(&counter),
            Err(ControlError::IdentifierExhausted)
        );
        assert_eq!(counter.load(Ordering::Relaxed), u64::MAX);
    }
}
