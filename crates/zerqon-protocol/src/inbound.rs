//! Account for owned inbound frame buffers across connections.
//!
//! Reservations cover wire and decoded buffers, not process RSS, allocator
//! overhead, fixed decoder workspace or explicit copies made by callers.
//! Overload is rejected immediately. Errors and cancellation require closing the
//! connection: part of a frame may already have been consumed.

use crate::codec::{Error as ProtocolError, get_varint};
use std::{
    fmt,
    io::{self, Read},
    sync::{Arc, Mutex, MutexGuard},
};
use tokio::io::{AsyncRead, AsyncReadExt};

/// A consistent snapshot of one account's reservations, not allocated RSS.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BudgetSnapshot {
    pub limit_bytes: usize,
    pub used_bytes: usize,
    pub peak_bytes: usize,
    /// Attempts rejected by this account's limit; saturates at `u64::MAX`.
    pub rejections: u64,
}

/// Invalid global or connection limits.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BudgetConfigError {
    #[error("packet memory limit must be nonzero")]
    Zero,
    #[error("connection packet memory limit exceeds the global limit")]
    ConnectionExceedsGlobal,
    #[error("packet memory limit exceeds the supported address range")]
    Overflow,
}

struct Account(Mutex<BudgetSnapshot>);

impl Account {
    fn new(limit_bytes: usize) -> Self {
        Self(Mutex::new(BudgetSnapshot {
            limit_bytes,
            used_bytes: 0,
            peak_bytes: 0,
            rejections: 0,
        }))
    }
    fn lock(&self) -> MutexGuard<'_, BudgetSnapshot> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Shared global account. Clones refer to the same limit and reservations.
#[derive(Clone)]
pub struct PacketBudget {
    account: Arc<Account>,
}

/// One connection's account. Cloning does not create another allowance.
#[derive(Clone)]
pub struct ConnectionBudget {
    global: Arc<Account>,
    local: Arc<Account>,
}

impl PacketBudget {
    /// Accepts a nonzero byte limit no greater than `isize::MAX`.
    pub fn new(bytes: usize) -> Result<Self, BudgetConfigError> {
        validate_limit(bytes)?;
        Ok(Self {
            account: Arc::new(Account::new(bytes)),
        })
    }
    /// Creates a distinct connection account whose limit cannot exceed global.
    pub fn connection(&self, bytes: usize) -> Result<ConnectionBudget, BudgetConfigError> {
        validate_limit(bytes)?;
        if bytes > self.account.lock().limit_bytes {
            return Err(BudgetConfigError::ConnectionExceedsGlobal);
        }
        Ok(ConnectionBudget {
            global: Arc::clone(&self.account),
            local: Arc::new(Account::new(bytes)),
        })
    }
    pub fn snapshot(&self) -> BudgetSnapshot {
        *self.account.lock()
    }
}

impl ConnectionBudget {
    pub fn snapshot(&self) -> BudgetSnapshot {
        *self.local.lock()
    }

    fn reserve(&self, bytes: usize) -> Result<Lease, ReadFrameError> {
        let mut global = self.global.lock();
        let mut local = self.local.lock();
        if bytes > local.limit_bytes - local.used_bytes {
            local.rejections = local.rejections.saturating_add(1);
            return Err(ReadFrameError::Budget {
                scope: BudgetScope::Connection,
                requested: bytes,
                limit: local.limit_bytes,
            });
        }
        if bytes > global.limit_bytes - global.used_bytes {
            global.rejections = global.rejections.saturating_add(1);
            return Err(ReadFrameError::Budget {
                scope: BudgetScope::Global,
                requested: bytes,
                limit: global.limit_bytes,
            });
        }
        global.used_bytes += bytes;
        local.used_bytes += bytes;
        global.peak_bytes = global.peak_bytes.max(global.used_bytes);
        local.peak_bytes = local.peak_bytes.max(local.used_bytes);
        Ok(Lease {
            global: Arc::clone(&self.global),
            local: Arc::clone(&self.local),
            bytes,
        })
    }
}

struct Lease {
    global: Arc<Account>,
    local: Arc<Account>,
    bytes: usize,
}
impl Lease {
    fn release(&mut self, bytes: usize) {
        let mut global = self.global.lock();
        let mut local = self.local.lock();
        global.used_bytes -= bytes;
        local.used_bytes -= bytes;
        self.bytes -= bytes;
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        self.release(self.bytes);
    }
}

fn validate_limit(bytes: usize) -> Result<(), BudgetConfigError> {
    if bytes == 0 {
        return Err(BudgetConfigError::Zero);
    }
    if bytes > isize::MAX as usize {
        return Err(BudgetConfigError::Overflow);
    }
    Ok(())
}

/// An immutable payload that retains both reservations until it is dropped.
/// Share the whole packet with `Arc<InboundPacket>` when needed. Borrowing the
/// payload does not account for additional copies allocated by the caller.
///
/// ```compile_fail
/// use zerqon_protocol::inbound::InboundPacket;
/// fn detach(packet: InboundPacket) -> Vec<u8> { packet.into_vec() }
/// ```
pub struct InboundPacket {
    payload: Box<[u8]>,
    _lease: Lease,
}
impl InboundPacket {
    pub fn as_slice(&self) -> &[u8] {
        &self.payload
    }
}
impl std::ops::Deref for InboundPacket {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        self.as_slice()
    }
}
impl fmt::Debug for InboundPacket {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InboundPacket")
            .field("len", &self.payload.len())
            .finish_non_exhaustive()
    }
}

/// The account that refused a complete buffer reservation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BudgetScope {
    Global,
    Connection,
}

/// Protocol, quota and allocation failures stay distinguishable to the caller.
#[derive(Debug, thiserror::Error)]
pub enum ReadFrameError {
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    #[error("{scope:?} packet memory budget cannot reserve {requested} bytes (limit {limit})")]
    Budget {
        scope: BudgetScope,
        requested: usize,
        limit: usize,
    },
    #[error("packet buffer allocation failed: {0}")]
    Allocation(#[from] std::collections::TryReserveError),
}
impl From<io::Error> for ReadFrameError {
    fn from(error: io::Error) -> Self {
        Self::Protocol(error.into())
    }
}

fn invalid(message: &'static str) -> ReadFrameError {
    ProtocolError::Invalid(message).into()
}

fn buffer(bytes: usize) -> Result<Box<[u8]>, ReadFrameError> {
    let mut data = Vec::new();
    data.try_reserve_exact(bytes)?;
    data.resize(bytes, 0);
    Ok(data.into_boxed_slice())
}

/// Reads one Minecraft frame while charging global and connection quotas.
/// No dynamic frame allocation occurs before both complete reservations succeed.
/// A compressed frame reserves its wire body plus decoded size together; there
/// is no wait or upgrade while holding a partially admitted frame.
///
/// The wire charge is released after decompression. The decoded charge moves
/// with the returned packet. On any error or cancellation, close the connection
/// instead of retrying this operation on its partially consumed byte stream.
pub async fn read_frame_budgeted<R: AsyncRead + Unpin>(
    reader: &mut R,
    max_len: usize,
    compression: Option<usize>,
    budget: &ConnectionBudget,
) -> Result<InboundPacket, ReadFrameError> {
    let mut len = 0;
    let mut finished = false;
    for index in 0..3 {
        let byte = reader.read_u8().await?;
        len |= usize::from(byte & 0x7f) << (index * 7);
        if byte & 0x80 == 0 {
            finished = true;
            break;
        }
    }
    if !finished || len == 0 || len > max_len {
        return Err(invalid("invalid frame size"));
    }
    let Some(threshold) = compression else {
        return read_payload(reader, len, budget).await;
    };
    let mut header = [0_u8; 5];
    let mut count = 0;
    loop {
        if count == len {
            return Err(invalid("truncated packet"));
        }
        let byte = reader.read_u8().await?;
        header[count] = byte;
        count += 1;
        if byte & 0x80 == 0 || count == header.len() {
            break;
        }
    }
    let expected = usize::try_from(get_varint(&mut &header[..count])?)
        .map_err(|_| invalid("negative decompressed size"))?;
    let wire_len = len - count;
    if expected == 0 {
        if wire_len == 0 || wire_len >= threshold {
            return Err(invalid("uncompressed packet above threshold"));
        }
        return read_payload(reader, wire_len, budget).await;
    }
    if expected < threshold || expected > max_len {
        return Err(invalid("invalid decompressed size"));
    }
    if wire_len == 0 {
        return Err(invalid("compression length mismatch"));
    }
    let required = wire_len
        .checked_add(expected)
        .ok_or_else(|| invalid("frame size overflow"))?;
    let mut lease = budget.reserve(required)?;
    let mut wire = buffer(wire_len)?;
    reader.read_exact(&mut wire).await?;
    let mut payload = buffer(expected)?;
    let mut decoder = flate2::read::ZlibDecoder::new(wire.as_ref());
    if let Err(error) = decoder.read_exact(&mut payload) {
        if error.kind() == io::ErrorKind::UnexpectedEof {
            return Err(invalid("compression length mismatch"));
        }
        return Err(error.into());
    }
    let mut extra = [0_u8; 1];
    if decoder.read(&mut extra)? != 0 || decoder.total_in() as usize != wire_len {
        return Err(invalid("compression length mismatch"));
    }
    drop(decoder);
    drop(wire);
    lease.release(wire_len);
    Ok(InboundPacket {
        payload,
        _lease: lease,
    })
}

async fn read_payload<R: AsyncRead + Unpin>(
    reader: &mut R,
    len: usize,
    budget: &ConnectionBudget,
) -> Result<InboundPacket, ReadFrameError> {
    let lease = budget.reserve(len)?;
    let mut payload = buffer(len)?;
    reader.read_exact(&mut payload).await?;
    Ok(InboundPacket {
        payload,
        _lease: lease,
    })
}
