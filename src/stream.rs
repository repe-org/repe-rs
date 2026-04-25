//! Backpressure-controlled streaming over REPE notifies.
//!
//! When you need to push a large payload from the server to a peer
//! over REPE (multi-GB capture files, log streams, query results
//! that don't fit in one message), the protocol gives you `notify`
//! frames but no flow control. This module adds the missing pieces:
//!
//! * **ACK-driven window credit.** The producer waits before sending
//!   if the receiver is more than `window_bytes` behind on ACKs;
//!   when an ACK arrives the producer wakes and resumes.
//! * **Idle watchdog.** A transfer that has gone silent for longer
//!   than `idle_timeout` is force-cancelled so a wedged peer can't
//!   leak server resources.
//! * **Replay ring + reconnect.** The producer keeps a byte-bounded
//!   ring of recently-emitted bodies; on a brief WS disconnect it
//!   parks until the peer reconnects and calls
//!   [`TransferControl::request_resume`] with a `last_received_offset`,
//!   then re-emits the ring tail through the new peer.
//!
//! The protocol itself (the wire shape of `transfer_begin` /
//! `file_chunk` / `transfer_ack` / etc.) is up to you: this module
//! deals only in offsets, ACKs, and opaque body bytes. See the
//! soul-rs `WsSink` for one concrete embedding.
//!
//! ## Lifecycle
//!
//! 1. Build a [`TransferControl`] with a window size and replay ring
//!    capacity. Install the initial peer with [`set_peer`].
//! 2. Register the control under your transfer-id type in a
//!    [`TransferRegistry`] so inbound ACK / cancel / resume handlers
//!    can find it.
//! 3. The producer thread, for each chunk:
//!    * [`wait_for_credit`] for `chunk_len` bytes,
//!    * encode the body and call [`push_replay`] with the logical
//!      `data_len` of the chunk and the wire body,
//!    * read the current [`peer`] and call `peer.send_notify(...)`,
//!    * on `Ok` call [`record_sent(new_offset)`],
//!    * on `Err(PeerSendError::Disconnected)` call
//!      [`wait_for_reconnect`]; on
//!      [`ReconnectOutcome::ResumeReady`] re-emit ring chunks
//!      starting at the returned `resume_at_offset` through the new
//!      peer.
//! 4. Inbound ACK handlers call [`record_ack`]; cancel calls
//!    [`cancel`]; resume calls [`request_resume`].
//! 5. At each file (or whatever your chunked unit is) boundary, call
//!    [`advance_to_file`] to reset offsets and clear the ring.
//! 6. When the transfer ends, call [`TransferRegistry::unregister`].
//!
//! [`set_peer`]: TransferControl::set_peer
//! [`peer`]: TransferControl::peer
//! [`record_sent(new_offset)`]: TransferControl::record_sent
//! [`wait_for_credit`]: TransferControl::wait_for_credit
//! [`push_replay`]: TransferControl::push_replay
//! [`wait_for_reconnect`]: TransferControl::wait_for_reconnect
//! [`record_ack`]: TransferControl::record_ack
//! [`cancel`]: TransferControl::cancel
//! [`request_resume`]: TransferControl::request_resume
//! [`advance_to_file`]: TransferControl::advance_to_file

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};

use crate::peer::PeerHandle;

/// Default in-flight wire bytes per transfer the producer allows
/// before it must wait on receiver ACKs. 64 MiB is a reasonable
/// starting point for LAN-class links; lower it on slow links.
pub const DEFAULT_WINDOW_BYTES: u64 = 64 * 1024 * 1024;

/// Default per-chunk credit timeout. If [`TransferControl::wait_for_credit`]
/// hasn't been released by an ACK in this long, the producer should
/// abort the transfer.
pub const DEFAULT_BACKPRESSURE_TIMEOUT: Duration = Duration::from_secs(30);

/// Default idle-transfer threshold. If neither a chunk has been sent
/// nor an ACK received in this long, the watchdog force-cancels the
/// transfer.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// Default replay ring capacity (per active file). 64 MiB worth of
/// recent bodies, FIFO-evicted; sized so a brief WS drop can replay
/// the whole window.
pub const DEFAULT_REPLAY_RING_BYTES: u64 = 64 * 1024 * 1024;

/// Default reconnect window. After a disconnect the producer parks
/// on [`TransferControl::wait_for_reconnect`] for at most this long
/// before giving up.
pub const DEFAULT_RECONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Per-transfer control: credit window, ACK accounting, replay ring,
/// peer slot, cancel flag, idle timestamps.
///
/// Cloned via `Arc` between the producer thread and the inbound
/// ACK / cancel / resume handlers. Protected by a single mutex with
/// one condvar; the mutex is held only for counter / ring updates so
/// the lock is uncontended in practice.
pub struct TransferControl {
    inner: Mutex<TransferControlInner>,
    cv: Condvar,
}

struct TransferControlInner {
    window_bytes: u64,
    /// Wire bytes emitted for the *currently-active* file. Resets at
    /// each [`TransferControl::advance_to_file`] call so we don't
    /// have to thread per-file offsets through the producer.
    sent_offset: u64,
    /// Latest offset the receiver has ACKed for the current file.
    /// Capped to `sent_offset` so a stale or malicious ACK can't
    /// grow the window beyond what's actually emitted.
    acked_offset: u64,
    current_file_index: u32,
    /// Set when cancel arrives. Sticky: once set, the producer
    /// cannot recover.
    cancelled: Option<String>,
    last_chunk_at: Instant,
    last_ack_at: Instant,
    /// Replay ring, scoped to the currently-active file. Cleared at
    /// each `advance_to_file` boundary.
    replay: ReplayRing,
    /// Current peer the producer should target. Replaced by
    /// [`TransferControl::request_resume`] on reconnect; the
    /// producer reads it each time it sends so the swap is observed
    /// immediately.
    peer: Option<PeerHandle>,
    /// Pending resume request. Consumed by the producer once it
    /// observes [`ReconnectOutcome::ResumeReady`].
    pending_resume: Option<PendingResume>,
}

/// Resume request published by the inbound resume handler and
/// consumed by the producer thread on its next call to
/// [`TransferControl::wait_for_reconnect`].
///
/// The new peer is *not* carried here: [`TransferControl::request_resume`]
/// installs it directly into the control's peer slot, and the
/// producer reads the current peer via [`TransferControl::peer`] on
/// every send. This keeps a single source of truth, so a second
/// resume during replay (rare but legal) doesn't leave the producer
/// looking at a stale `new_peer` field.
#[derive(Debug, Clone)]
pub struct PendingResume {
    pub resume_at_offset: u64,
}

/// Replay buffer of the most-recently-emitted bodies for the
/// currently-active file. Bounded by `capacity_bytes`; older entries
/// are evicted from the front when the bound is exceeded.
struct ReplayRing {
    chunks: VecDeque<RingChunk>,
    bytes_held: u64,
    capacity_bytes: u64,
}

/// One entry in the replay ring.
///
/// * `offset` and `data_len` are in the embedder's *logical* byte
///   domain (whatever monotonic counter the producer uses for ACKs and
///   for `last_received_offset` on resume). Successive chunks satisfy
///   `next.offset == prev.offset + prev.data_len`.
/// * `body_bytes` is the exact wire body the producer pushed onto the
///   peer (whatever encoding it used); replay is a straight resend,
///   not a re-encode. Its length may exceed `data_len` because it can
///   include framing/envelope overhead, and that's why the ring tracks
///   the two separately.
#[derive(Clone)]
pub struct RingChunk {
    pub offset: u64,
    pub data_len: u64,
    pub last: bool,
    pub body_bytes: Arc<Vec<u8>>,
}

impl ReplayRing {
    fn new(capacity_bytes: u64) -> Self {
        Self {
            chunks: VecDeque::new(),
            bytes_held: 0,
            capacity_bytes,
        }
    }

    fn push(&mut self, offset: u64, data_len: u64, last: bool, body_bytes: Vec<u8>) {
        // Successive chunks must abut in the logical-offset domain.
        // The ring's invariant for `replay_from(o)` and `covers(o)`
        // is that the chunks form a contiguous run, so a gap or an
        // overlap here means a coding mistake (the producer's offset
        // counter drifted from the data it actually pushed).
        debug_assert!(
            self.chunks
                .back()
                .is_none_or(|c| offset == c.offset + c.data_len),
            "ReplayRing::push: non-contiguous offset {offset} (last ended at {:?})",
            self.chunks.back().map(|c| c.offset + c.data_len),
        );
        let wire_len = body_bytes.len() as u64;
        // Single oversized chunk: keep it as the sole entry rather
        // than spinning forever evicting the only thing we have.
        self.chunks.push_back(RingChunk {
            offset,
            data_len,
            last,
            body_bytes: Arc::new(body_bytes),
        });
        // bytes_held is the wire-byte resource we're bounding (what
        // a re-emit would actually push back over the socket), so it
        // tracks body_bytes.len(), not data_len.
        self.bytes_held = self.bytes_held.saturating_add(wire_len);
        while self.bytes_held > self.capacity_bytes && self.chunks.len() > 1 {
            if let Some(front) = self.chunks.pop_front() {
                self.bytes_held = self
                    .bytes_held
                    .saturating_sub(front.body_bytes.len() as u64);
            }
        }
    }

    fn clear(&mut self) {
        self.chunks.clear();
        self.bytes_held = 0;
    }

    fn highest_end_offset(&self) -> Option<u64> {
        self.chunks.back().map(|c| c.offset + c.data_len)
    }

    /// True if `offset` is at or after a stored chunk boundary AND
    /// not past everything the ring holds. Resume requires the
    /// requested offset to land at a chunk boundary; we return true
    /// when there is a chunk at `offset` exactly OR when the ring
    /// is empty and `offset == 0` (transfer just started) OR when
    /// `offset` matches the ring's trailing edge (receiver is fully
    /// caught up with everything we've emitted).
    fn covers(&self, offset: u64) -> bool {
        if self.chunks.is_empty() {
            return offset == 0;
        }
        for chunk in &self.chunks {
            if chunk.offset == offset {
                return true;
            }
        }
        self.highest_end_offset() == Some(offset)
    }

    /// Iterator over chunks at or after `offset`. Used by the
    /// producer during resume to re-emit the missing tail.
    fn replay_from(&self, offset: u64) -> Vec<RingChunk> {
        self.chunks
            .iter()
            .filter(|c| c.offset >= offset)
            .cloned()
            .collect()
    }
}

/// Why [`TransferControl::wait_for_credit`] returned without granting
/// credit. The caller maps this to an error so the producer pipeline
/// aborts cleanly.
#[derive(Debug)]
pub enum CreditError {
    /// Cancel arrived (sticky). The contained string is whatever the
    /// caller passed to [`TransferControl::cancel`].
    Cancelled(String),
    /// `wait_for_credit` reached its deadline without `(sent - acked)`
    /// dropping below the window threshold.
    Timeout,
}

impl std::fmt::Display for CreditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CreditError::Cancelled(reason) => write!(f, "transfer cancelled: {reason}"),
            CreditError::Timeout => write!(f, "credit timeout"),
        }
    }
}

impl std::error::Error for CreditError {}

/// Outcome of [`TransferControl::wait_for_reconnect`]. The producer
/// uses this to decide whether to swap in a new peer + replay, give
/// up because of cancel, or give up because the reconnect window
/// expired.
#[derive(Debug)]
pub enum ReconnectOutcome {
    /// A resume request arrived. The carried [`PendingResume`] is
    /// already consumed from the control's internal slot; the new
    /// peer is installed and observable via [`TransferControl::peer`].
    ResumeReady(PendingResume),
    /// Cancel arrived (sticky). The reason mirrors
    /// [`CreditError::Cancelled`].
    Cancelled(String),
    /// No resume request landed before the deadline.
    Timeout,
}

/// Why [`TransferControl::request_resume`] rejected a resume. The
/// embedder maps these to wire strings on its resume reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeRejection {
    /// Resume was for a file other than the one the producer is
    /// currently emitting.
    WrongFileIndex { requested: u32, current: u32 },
    /// Requested offset is outside the replay ring.
    OutOfWindow,
    /// Transfer is already cancelled; can't be resumed.
    Cancelled,
}

impl ResumeRejection {
    /// Stable short string suitable for the wire `reason` field.
    pub fn reason(&self) -> &'static str {
        match self {
            ResumeRejection::WrongFileIndex { .. } => "wrong_file_index",
            ResumeRejection::OutOfWindow => "out_of_window",
            ResumeRejection::Cancelled => "transfer_cancelled",
        }
    }
}

impl TransferControl {
    /// Build a control with the default replay-ring capacity
    /// ([`DEFAULT_REPLAY_RING_BYTES`]).
    pub fn new(window_bytes: u64) -> Arc<Self> {
        Self::with_replay_capacity(window_bytes, DEFAULT_REPLAY_RING_BYTES)
    }

    /// Build a control with an explicit replay-ring capacity. The
    /// producer pushes every emitted body into this ring; the resume
    /// handler reads from it to validate `last_received_offset` and
    /// the producer reads from it to re-emit the tail on reconnect.
    pub fn with_replay_capacity(window_bytes: u64, replay_capacity_bytes: u64) -> Arc<Self> {
        let now = Instant::now();
        Arc::new(Self {
            inner: Mutex::new(TransferControlInner {
                window_bytes,
                sent_offset: 0,
                acked_offset: 0,
                current_file_index: 0,
                cancelled: None,
                last_chunk_at: now,
                last_ack_at: now,
                replay: ReplayRing::new(replay_capacity_bytes),
                peer: None,
                pending_resume: None,
            }),
            cv: Condvar::new(),
        })
    }

    /// Install or replace the peer the producer should target.
    /// Called once when a transfer starts; called again by
    /// [`Self::request_resume`] on reconnect.
    pub fn set_peer(&self, peer: PeerHandle) {
        let mut g = self.inner.lock().expect("TransferControl mutex poisoned");
        g.peer = Some(peer);
    }

    /// Clone of the current peer. The producer reads this on every
    /// send so a swap from `request_resume` is observed immediately.
    pub fn peer(&self) -> Option<PeerHandle> {
        self.inner
            .lock()
            .expect("TransferControl mutex poisoned")
            .peer
            .clone()
    }

    /// Push one wire body into the replay ring. The producer calls
    /// this *before* sending so a `Disconnected` error doesn't leave
    /// the ring missing the failed body.
    ///
    /// `offset` and `data_len` are in the embedder's logical-offset
    /// domain (the same domain ACKs use). `body_bytes` is the
    /// actual wire payload, which may be longer than `data_len` if
    /// the encoding adds envelope overhead. The ring's eviction
    /// budget is in wire bytes; trailing-edge resume math is in
    /// logical bytes.
    pub fn push_replay(&self, offset: u64, data_len: u64, last: bool, body_bytes: Vec<u8>) {
        let mut g = self.inner.lock().expect("TransferControl mutex poisoned");
        g.replay.push(offset, data_len, last, body_bytes);
    }

    /// Snapshot the ring chunks at or after `offset`. Returns owned
    /// `Arc<Vec<u8>>` clones so the caller can re-emit without
    /// holding the lock.
    pub fn replay_chunks_from(&self, offset: u64) -> Vec<RingChunk> {
        let g = self.inner.lock().expect("TransferControl mutex poisoned");
        g.replay.replay_from(offset)
    }

    /// Validate and stage a resume. Validation:
    ///
    /// * Must match `current_file_index`.
    /// * Must not be cancelled.
    /// * `last_received_offset` must be inside the replay ring (or 0
    ///   for a fresh file, or the trailing edge of the ring).
    ///
    /// On success, the new peer + requested offset are installed and
    /// the condvar is notified so a producer parked in
    /// [`Self::wait_for_reconnect`] wakes.
    pub fn request_resume(
        &self,
        new_peer: PeerHandle,
        file_index: u32,
        last_received_offset: u64,
    ) -> Result<u64, ResumeRejection> {
        let mut g = self.inner.lock().expect("TransferControl mutex poisoned");
        if g.cancelled.is_some() {
            return Err(ResumeRejection::Cancelled);
        }
        if file_index != g.current_file_index {
            return Err(ResumeRejection::WrongFileIndex {
                requested: file_index,
                current: g.current_file_index,
            });
        }
        if !g.replay.covers(last_received_offset) {
            return Err(ResumeRejection::OutOfWindow);
        }
        g.peer = Some(new_peer);
        g.pending_resume = Some(PendingResume {
            resume_at_offset: last_received_offset,
        });
        // Refresh progress timestamps so the watchdog doesn't fire
        // immediately after a long disconnect.
        let now = Instant::now();
        g.last_chunk_at = now;
        g.last_ack_at = now;
        // ACKed up to the resume point: receiver confirmed those
        // bytes. Releases credit space in the new send window.
        if last_received_offset > g.acked_offset && last_received_offset <= g.sent_offset {
            g.acked_offset = last_received_offset;
        }
        self.cv.notify_all();
        Ok(last_received_offset)
    }

    /// Park until a resume request, cancel, or `timeout`. The
    /// producer calls this once `peer.send_notify(...)` returns
    /// [`crate::PeerSendError::Disconnected`].
    ///
    /// On [`ReconnectOutcome::ResumeReady`] the staged
    /// [`PendingResume`] is consumed from the control's internal
    /// slot and returned to the caller, so a second concurrent
    /// `request_resume` cannot race ahead of the producer.
    pub fn wait_for_reconnect(&self, timeout: Duration) -> ReconnectOutcome {
        let deadline = Instant::now() + timeout;
        let mut g = self.inner.lock().expect("TransferControl mutex poisoned");
        loop {
            if let Some(reason) = g.cancelled.clone() {
                return ReconnectOutcome::Cancelled(reason);
            }
            if let Some(pending) = g.pending_resume.take() {
                return ReconnectOutcome::ResumeReady(pending);
            }
            let now = Instant::now();
            if now >= deadline {
                return ReconnectOutcome::Timeout;
            }
            let (gg, _) = self
                .cv
                .wait_timeout(g, deadline - now)
                .expect("TransferControl mutex poisoned");
            g = gg;
        }
    }

    /// Block until enough credit is available to send `chunk_len`
    /// bytes, or until `deadline`. Returns immediately on
    /// [`CreditError::Cancelled`].
    ///
    /// Concurrency note: this design assumes a single producer per
    /// transfer; credit reservation is not atomic with the wait. If
    /// you ever add a second concurrent producer, move
    /// [`Self::record_sent`] accounting under the same lock as the
    /// wait.
    pub fn wait_for_credit(&self, chunk_len: u64, deadline: Instant) -> Result<(), CreditError> {
        let mut guard = self.inner.lock().expect("TransferControl mutex poisoned");
        loop {
            if let Some(reason) = guard.cancelled.clone() {
                return Err(CreditError::Cancelled(reason));
            }
            let in_flight = guard.sent_offset.saturating_sub(guard.acked_offset);
            // A single oversized chunk (chunk_len > window_bytes)
            // would otherwise spin forever. Clamp so the first
            // chunk always passes; the practical case
            // (chunk_size <= window_bytes) is unaffected.
            if in_flight == 0 || in_flight + chunk_len <= guard.window_bytes {
                return Ok(());
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(CreditError::Timeout);
            }
            let timeout = deadline - now;
            let (g, _) = self
                .cv
                .wait_timeout(guard, timeout)
                .expect("TransferControl mutex poisoned");
            guard = g;
        }
    }

    /// Bump the sent offset and refresh the watchdog timestamp.
    ///
    /// Call this **only after** `peer.send_notify(...)` returned
    /// `Ok` for the chunk ending at `new_offset`. Calling it after a
    /// failed send (e.g. `PeerSendError::Disconnected`) races
    /// `sent_offset` ahead of what the receiver could actually ACK,
    /// permanently widening `sent - acked` and stalling the credit
    /// gate.
    pub fn record_sent(&self, new_offset: u64) {
        let mut guard = self.inner.lock().expect("TransferControl mutex poisoned");
        if new_offset > guard.sent_offset {
            guard.sent_offset = new_offset;
        }
        guard.last_chunk_at = Instant::now();
    }

    /// Apply an ACK from the receiver. Stale or out-of-order ACKs
    /// (older `file_index`, or `offset <= acked_offset`) are folded
    /// into the watchdog timestamp but don't release credit.
    pub fn record_ack(&self, file_index: u32, received_through_offset: u64) {
        let mut guard = self.inner.lock().expect("TransferControl mutex poisoned");
        guard.last_ack_at = Instant::now();
        if file_index == guard.current_file_index {
            // Cap to sent_offset: a misbehaving receiver can't grow
            // the window past what we've actually emitted.
            let capped = received_through_offset.min(guard.sent_offset);
            if capped > guard.acked_offset {
                guard.acked_offset = capped;
                self.cv.notify_all();
            }
        }
    }

    /// Mark the transfer cancelled. Sticky; subsequent calls are
    /// no-ops so the first reason wins.
    pub fn cancel(&self, reason: impl Into<String>) {
        let mut guard = self.inner.lock().expect("TransferControl mutex poisoned");
        if guard.cancelled.is_none() {
            guard.cancelled = Some(reason.into());
            self.cv.notify_all();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner
            .lock()
            .expect("TransferControl mutex poisoned")
            .cancelled
            .is_some()
    }

    pub fn cancel_reason(&self) -> Option<String> {
        self.inner
            .lock()
            .expect("TransferControl mutex poisoned")
            .cancelled
            .clone()
    }

    /// Reset credit counters and clear the replay ring at a file
    /// boundary. The protocol convention is that the receiver's
    /// "file complete" message implicitly ACKs everything in the
    /// file, so the next file starts with a fresh window.
    pub fn advance_to_file(&self, next_file_index: u32) {
        let mut guard = self.inner.lock().expect("TransferControl mutex poisoned");
        guard.current_file_index = next_file_index;
        guard.sent_offset = 0;
        guard.acked_offset = 0;
        guard.replay.clear();
        // A pending resume request that was for the old file is now
        // stale; drop it so the producer doesn't accidentally rewind.
        guard.pending_resume = None;
        let now = Instant::now();
        guard.last_chunk_at = now;
        guard.last_ack_at = now;
        self.cv.notify_all();
    }

    /// Snapshot for the watchdog: `(last_chunk_at, last_ack_at)`.
    pub fn timestamps(&self) -> (Instant, Instant) {
        let g = self.inner.lock().expect("TransferControl mutex poisoned");
        (g.last_chunk_at, g.last_ack_at)
    }

    /// Snapshot for tests / observability: `(sent_offset, acked_offset)`.
    pub fn offsets(&self) -> (u64, u64) {
        let g = self.inner.lock().expect("TransferControl mutex poisoned");
        (g.sent_offset, g.acked_offset)
    }
}

/// Table of in-flight transfers indexed by an embedder-chosen key
/// type `K` (typically a transfer-id newtype).
///
/// The embedder owns the `Arc<TransferRegistry<K>>` and shares it
/// with both the inbound handlers (ACK / cancel / resume) and the
/// idle watchdog.
pub struct TransferRegistry<K: Hash + Eq + Copy + Send + Sync + 'static> {
    map: Mutex<HashMap<K, Arc<TransferControl>>>,
}

impl<K: Hash + Eq + Copy + Send + Sync + 'static> Default for TransferRegistry<K> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Hash + Eq + Copy + Send + Sync + 'static> TransferRegistry<K> {
    pub fn new() -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
        }
    }

    pub fn register(&self, key: K, control: Arc<TransferControl>) {
        self.map
            .lock()
            .expect("TransferRegistry mutex poisoned")
            .insert(key, control);
    }

    /// Remove the entry. Returns the dropped control so callers that
    /// want to invariant-check ("we were registered") can.
    pub fn unregister(&self, key: K) -> Option<Arc<TransferControl>> {
        self.map
            .lock()
            .expect("TransferRegistry mutex poisoned")
            .remove(&key)
    }

    pub fn get(&self, key: K) -> Option<Arc<TransferControl>> {
        self.map
            .lock()
            .expect("TransferRegistry mutex poisoned")
            .get(&key)
            .cloned()
    }

    /// Snapshot for the watchdog. Returns `(key, control)` pairs so
    /// the watchdog can scan without holding the registry lock
    /// during timestamp checks.
    pub fn snapshot(&self) -> Vec<(K, Arc<TransferControl>)> {
        self.map
            .lock()
            .expect("TransferRegistry mutex poisoned")
            .iter()
            .map(|(k, v)| (*k, v.clone()))
            .collect()
    }

    /// Number of currently-registered transfers. Useful in tests.
    pub fn len(&self) -> usize {
        self.map
            .lock()
            .expect("TransferRegistry mutex poisoned")
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Spawn a background thread that scans `registry` periodically and
/// fires [`TransferControl::cancel`]("transfer idle") on any
/// transfer whose last chunk and last ACK are both older than
/// `idle_timeout`.
///
/// Returns immediately. The watchdog holds a [`Weak`] reference to
/// the registry, so it does not keep the registry alive on its own:
/// once every strong reference the embedder holds is dropped, the
/// next tick's `Weak::upgrade` returns `None` and the thread exits
/// cleanly. For a process-wide singleton this just means the
/// thread lives for the process; for embedders that build short-
/// lived registries (per-test, per-tenant), dropping the registry
/// terminates the watchdog within `tick` (clamped to [1 s, 5 s]).
pub fn spawn_watchdog<K>(registry: Arc<TransferRegistry<K>>, idle_timeout: Duration)
where
    K: Hash + Eq + Copy + Send + Sync + 'static,
{
    let weak = Arc::downgrade(&registry);
    drop(registry);
    std::thread::Builder::new()
        .name("repe-stream-watchdog".to_string())
        .spawn(move || watchdog_loop(weak, idle_timeout))
        .expect("spawn idle-transfer watchdog");
}

fn watchdog_loop<K>(registry: Weak<TransferRegistry<K>>, idle_timeout: Duration)
where
    K: Hash + Eq + Copy + Send + Sync + 'static,
{
    // tick = clamp(idle_timeout / 4, 1 s, 5 s). The 1/4 factor keeps
    // worst-case kill latency under 2*idle_timeout; the [1 s, 5 s]
    // clamp keeps idle CPU low on big timeouts and bounds the kill
    // floor on tiny ones. With a 60 s default this works out to 5 s.
    let tick = (idle_timeout / 4)
        .min(Duration::from_secs(5))
        .max(Duration::from_secs(1));
    loop {
        std::thread::sleep(tick);
        let Some(registry) = registry.upgrade() else {
            // Embedder dropped its last strong ref; the registry
            // (and any TransferControl entries it held) is gone.
            return;
        };
        let snapshot = registry.snapshot();
        // Drop the registry strong ref before we enter the per-
        // transfer loop so we don't artificially extend its life
        // across our own iteration.
        drop(registry);
        let now = Instant::now();
        for (_key, control) in snapshot {
            if control.is_cancelled() {
                continue;
            }
            let (last_chunk_at, last_ack_at) = control.timestamps();
            let last_progress = last_chunk_at.max(last_ack_at);
            if now.saturating_duration_since(last_progress) >= idle_timeout {
                control.cancel("transfer idle");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trivial PeerSink stand-in for tests that need a fresh
    /// PeerHandle. We never actually call `send_notify` on it.
    fn dummy_peer(id: u64) -> PeerHandle {
        use crate::peer::{NotifyBody, PeerId, PeerSendError, PeerSink};
        struct Dummy;
        impl PeerSink for Dummy {
            fn send_notify(
                &self,
                _method: &str,
                _body: NotifyBody,
            ) -> std::result::Result<(), PeerSendError> {
                Ok(())
            }
            fn is_connected(&self) -> bool {
                true
            }
        }
        PeerHandle::new(PeerId(id), Arc::new(Dummy))
    }

    #[test]
    fn wait_returns_immediately_when_window_has_room() {
        let ctl = TransferControl::new(1024);
        let deadline = Instant::now() + Duration::from_millis(50);
        ctl.wait_for_credit(512, deadline).unwrap();
    }

    #[test]
    fn wait_blocks_when_window_full_and_unblocks_on_ack() {
        let ctl = TransferControl::new(1024);
        ctl.record_sent(1024);

        let writer = Arc::clone(&ctl);
        let handle = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            writer.wait_for_credit(512, deadline)
        });

        std::thread::sleep(Duration::from_millis(50));
        ctl.record_ack(0, 512);

        let res = handle.join().expect("writer thread");
        res.expect("ack should release credit");
    }

    #[test]
    fn wait_returns_timeout_when_no_ack_arrives() {
        let ctl = TransferControl::new(1024);
        ctl.record_sent(1024);
        let deadline = Instant::now() + Duration::from_millis(80);
        let err = ctl.wait_for_credit(512, deadline).unwrap_err();
        assert!(matches!(err, CreditError::Timeout), "got {err:?}");
    }

    #[test]
    fn cancel_unblocks_waiters() {
        let ctl = TransferControl::new(1024);
        ctl.record_sent(1024);

        let writer = Arc::clone(&ctl);
        let handle = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            writer.wait_for_credit(512, deadline)
        });

        std::thread::sleep(Duration::from_millis(50));
        ctl.cancel("client cancelled");

        let err = handle.join().unwrap().unwrap_err();
        match err {
            CreditError::Cancelled(reason) => assert_eq!(reason, "client cancelled"),
            other => panic!("expected Cancelled, got {other:?}"),
        }
    }

    #[test]
    fn ack_for_wrong_file_index_is_ignored() {
        let ctl = TransferControl::new(1024);
        ctl.record_sent(800);
        ctl.record_ack(7, 600);
        let (sent, acked) = ctl.offsets();
        assert_eq!(sent, 800);
        assert_eq!(acked, 0);
    }

    #[test]
    fn ack_offset_is_capped_to_sent_offset() {
        let ctl = TransferControl::new(1024);
        ctl.record_sent(400);
        ctl.record_ack(0, 999_999);
        let (_, acked) = ctl.offsets();
        assert_eq!(acked, 400, "ack must not advance past actually-sent bytes");
    }

    #[test]
    fn advance_to_file_resets_offsets() {
        let ctl = TransferControl::new(1024);
        ctl.record_sent(900);
        ctl.record_ack(0, 800);
        ctl.advance_to_file(1);
        let (sent, acked) = ctl.offsets();
        assert_eq!(sent, 0);
        assert_eq!(acked, 0);
    }

    #[test]
    fn registry_round_trip() {
        #[derive(Hash, Eq, PartialEq, Copy, Clone)]
        struct Id(u64);
        let reg: TransferRegistry<Id> = TransferRegistry::new();
        let ctl = TransferControl::new(1024);
        let id = Id(424_242);
        reg.register(id, Arc::clone(&ctl));
        assert!(reg.get(id).is_some());
        let removed = reg.unregister(id).unwrap();
        assert!(Arc::ptr_eq(&removed, &ctl));
        assert!(reg.get(id).is_none());
    }

    #[test]
    fn oversized_chunk_does_not_deadlock_when_window_is_idle() {
        let ctl = TransferControl::new(1024);
        let deadline = Instant::now() + Duration::from_millis(50);
        ctl.wait_for_credit(8192, deadline).unwrap();
    }

    // --- replay ring + reconnect signalling --------------------

    #[test]
    fn replay_ring_evicts_oldest_when_full() {
        let mut ring = ReplayRing::new(100);
        ring.push(0, 40, false, vec![0u8; 40]);
        ring.push(40, 40, false, vec![0u8; 40]);
        assert_eq!(ring.bytes_held, 80);
        assert_eq!(ring.chunks.len(), 2);

        ring.push(80, 40, false, vec![0u8; 40]);
        assert_eq!(ring.bytes_held, 80);
        assert_eq!(ring.chunks.len(), 2);
        assert_eq!(ring.chunks.front().unwrap().offset, 40);
        assert_eq!(ring.chunks.back().unwrap().offset, 80);
    }

    #[test]
    fn replay_ring_keeps_single_oversized_chunk() {
        let mut ring = ReplayRing::new(50);
        ring.push(0, 200, false, vec![0u8; 200]);
        assert_eq!(ring.chunks.len(), 1);
        assert_eq!(ring.bytes_held, 200);
    }

    #[test]
    fn replay_ring_covers_offsets() {
        let mut ring = ReplayRing::new(1024);
        assert!(ring.covers(0));
        assert!(!ring.covers(8));

        ring.push(0, 16, false, vec![0u8; 16]);
        ring.push(16, 16, false, vec![0u8; 16]);
        assert!(ring.covers(0));
        assert!(ring.covers(16));
        assert!(!ring.covers(8));
        // Trailing edge.
        assert!(ring.covers(32));
        assert!(!ring.covers(64));
    }

    #[test]
    fn replay_ring_covers_trailing_edge_when_wire_bytes_exceed_data_len() {
        // Repro for the bug fixed in v2.3: with body_bytes longer
        // than data_len (envelope overhead), the trailing-edge match
        // must use data_len, not body_bytes.len().
        let mut ring = ReplayRing::new(1024);
        ring.push(0, 16, false, vec![0u8; 24]); // 8 bytes envelope
        ring.push(16, 16, true, vec![0u8; 24]);
        // Receiver fully caught up at offset 32 (= 16 + 16) should
        // be covered, even though wire length sums to 48.
        assert!(ring.covers(32));
        assert!(!ring.covers(48));
    }

    #[test]
    fn replay_chunks_from_returns_tail() {
        let mut ring = ReplayRing::new(1024);
        ring.push(0, 16, false, vec![0xAA; 16]);
        ring.push(16, 16, false, vec![0xBB; 16]);
        ring.push(32, 16, true, vec![0xCC; 16]);

        let from_16 = ring.replay_from(16);
        assert_eq!(from_16.len(), 2);
        assert_eq!(from_16[0].offset, 16);
        assert_eq!(from_16[1].offset, 32);
        assert!(from_16[1].last);
    }

    #[test]
    fn request_resume_validates_file_index() {
        let ctl = TransferControl::with_replay_capacity(1024, 1024);
        ctl.set_peer(dummy_peer(1));
        ctl.advance_to_file(2);
        ctl.push_replay(0, 16, false, vec![0u8; 16]);

        let new_peer = dummy_peer(2);
        let err = ctl.request_resume(new_peer, 5, 0).unwrap_err();
        match err {
            ResumeRejection::WrongFileIndex { requested, current } => {
                assert_eq!(requested, 5);
                assert_eq!(current, 2);
            }
            other => panic!("expected WrongFileIndex, got {other:?}"),
        }
    }

    #[test]
    fn request_resume_validates_offset_in_ring() {
        let ctl = TransferControl::with_replay_capacity(1024, 64);
        ctl.set_peer(dummy_peer(1));
        ctl.push_replay(0, 32, false, vec![0u8; 32]);
        ctl.push_replay(32, 32, false, vec![0u8; 32]);
        ctl.push_replay(64, 32, false, vec![0u8; 32]);
        // Ring capacity = 64; first chunk (offset 0) was evicted.
        let new_peer = dummy_peer(2);
        let err = ctl.request_resume(new_peer, 0, 0).unwrap_err();
        assert_eq!(err, ResumeRejection::OutOfWindow);
    }

    #[test]
    fn request_resume_signals_writer_via_condvar() {
        let ctl = TransferControl::new(1024);
        ctl.set_peer(dummy_peer(1));
        ctl.push_replay(0, 16, false, vec![0u8; 16]);

        let writer_ctl = Arc::clone(&ctl);
        let waiter =
            std::thread::spawn(move || writer_ctl.wait_for_reconnect(Duration::from_secs(2)));

        std::thread::sleep(Duration::from_millis(40));
        let new_peer = dummy_peer(2);
        ctl.request_resume(new_peer, 0, 0).unwrap();

        match waiter.join().unwrap() {
            ReconnectOutcome::ResumeReady(pending) => {
                assert_eq!(pending.resume_at_offset, 0);
            }
            other => panic!("expected ResumeReady, got {other:?}"),
        }
    }

    #[test]
    fn wait_for_reconnect_times_out() {
        let ctl = TransferControl::new(1024);
        match ctl.wait_for_reconnect(Duration::from_millis(40)) {
            ReconnectOutcome::Timeout => {}
            other => panic!("expected Timeout, got {other:?}"),
        }
    }

    #[test]
    fn wait_for_reconnect_returns_cancelled_when_cancelled() {
        let ctl = TransferControl::new(1024);
        ctl.cancel("client requested abort");
        match ctl.wait_for_reconnect(Duration::from_secs(1)) {
            ReconnectOutcome::Cancelled(reason) => {
                assert_eq!(reason, "client requested abort")
            }
            other => panic!("expected Cancelled, got {other:?}"),
        }
    }

    #[test]
    fn advance_to_file_clears_replay_ring() {
        let ctl = TransferControl::with_replay_capacity(1024, 1024);
        ctl.set_peer(dummy_peer(1));
        ctl.push_replay(0, 32, false, vec![0u8; 32]);
        let before = ctl.replay_chunks_from(0);
        assert_eq!(before.len(), 1);
        ctl.advance_to_file(1);
        let after = ctl.replay_chunks_from(0);
        assert!(after.is_empty(), "ring should be empty after file boundary");
    }

    #[test]
    fn watchdog_does_not_keep_registry_alive() {
        // The watchdog holds a Weak<TransferRegistry>, not an Arc, so
        // dropping the embedder's last strong ref must let the
        // registry actually deallocate. Without the Weak fix this
        // test would observe `weak.strong_count() != 0` indefinitely
        // because the watchdog thread would still be holding an Arc.
        #[derive(Hash, Eq, PartialEq, Copy, Clone)]
        struct Id(u64);

        let registry: Arc<TransferRegistry<Id>> = Arc::new(TransferRegistry::new());
        let weak = Arc::downgrade(&registry);
        spawn_watchdog(Arc::clone(&registry), Duration::from_millis(100));
        drop(registry);

        // Watchdog tick floors at 1 s. After 1.5 s the loop has run
        // at least once (upgrade -> None -> return) and the
        // registry's last refcount is released.
        std::thread::sleep(Duration::from_millis(1_500));
        assert_eq!(
            weak.strong_count(),
            0,
            "watchdog must not extend the registry's lifetime"
        );
    }

    #[test]
    fn watchdog_check_logic_matches_spawn_loop() {
        // Inline copy of the watchdog's per-transfer check; the
        // spawn_watchdog version is the same logic in a thread.
        let ctl = TransferControl::new(1024);
        let idle_timeout = Duration::from_millis(40);
        std::thread::sleep(idle_timeout + Duration::from_millis(20));

        let (last_chunk_at, last_ack_at) = ctl.timestamps();
        let last_progress = last_chunk_at.max(last_ack_at);
        let elapsed = Instant::now().saturating_duration_since(last_progress);
        assert!(elapsed >= idle_timeout);
        ctl.cancel("transfer idle");

        match ctl.cancel_reason() {
            Some(reason) => assert_eq!(reason, "transfer idle"),
            None => panic!("watchdog should have cancelled"),
        }
    }
}
