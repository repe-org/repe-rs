//! Size limits for a REPE WebSocket connection.
//!
//! WebSocket size limits are enforced by the **reader**. Both ends carry their
//! own thresholds, a writer consults none of them, and nothing in the handshake
//! communicates either side's choice. That asymmetry is the whole reason this
//! module exists, and it produces a failure that is easy to misread: an
//! oversized message leaves the sender cleanly, and the receiver's reader
//! rejects it and closes the connection. The sender observes a dropped socket,
//! not an error, and a client that reconnects and re-requests the same payload
//! reproduces it forever.
//!
//! [`WebSocketLimits`] therefore covers both directions:
//!
//! - the **incoming** thresholds this endpoint enforces while reading, and
//! - an **assumed** limit for the peer, checked before sending so an
//!   undeliverable message becomes a [`RepeError::MessageTooLarge`] instead of
//!   a silent disconnect.
//!
//! [`RepeError::MessageTooLarge`]: crate::RepeError::MessageTooLarge

/// Default assumed peer frame limit, and the default incoming frame limit.
///
/// 16 MiB is the underlying transport's own default, so for a REPE endpoint
/// talking to another REPE endpoint it is the peer's actual threshold unless
/// that peer was deliberately reconfigured.
pub const DEFAULT_MAX_FRAME_SIZE: usize = 16 << 20;

/// Default incoming message limit: 64 MiB, the underlying transport's default.
pub const DEFAULT_MAX_MESSAGE_SIZE: usize = 64 << 20;

/// Size limits for one WebSocket connection.
///
/// This is a repe-owned type rather than a re-export of the underlying
/// transport's configuration struct. repe exposes no transport types in its
/// public API, and keeping it that way means a transport version bump stays an
/// internal detail instead of a repe breaking change. It also keeps the surface
/// to the knobs that carry protocol meaning: buffer-tuning fields are
/// deliberately absent, which additionally avoids the transport's habit of
/// panicking on internally inconsistent buffer settings.
///
/// # Example
///
/// ```
/// use repe::WebSocketLimits;
///
/// // Accept larger inbound messages, and expect the peer to do the same.
/// let limits = WebSocketLimits::default()
///     .with_max_incoming_frame_size(Some(64 << 20))
///     .with_assumed_peer_frame_limit(Some(64 << 20));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WebSocketLimits {
    /// Largest single inbound frame this endpoint will accept, or `None` for
    /// unlimited. Enforced while reading.
    pub max_incoming_frame_size: Option<usize>,
    /// Largest reassembled inbound message this endpoint will accept, or `None`
    /// for unlimited. Enforced while reading.
    pub max_incoming_message_size: Option<usize>,
    /// Size above which an **outbound** message is refused with
    /// [`RepeError::MessageTooLarge`] rather than sent, or `None` to send
    /// whatever the application produces.
    ///
    /// This is an assumption the application configures, never a value learned
    /// from the peer: no WebSocket handshake field carries it. Set it to the
    /// peer's `max_incoming_frame_size` when that is known.
    ///
    /// Defaults to [`DEFAULT_MAX_FRAME_SIZE`], which is what an unconfigured
    /// peer enforces. That default cannot break a working deployment: before
    /// this type existed there was no way to raise a peer's inbound limit, so
    /// no message larger than it has ever been deliverable to a default peer.
    /// The guard converts a failure that was already happening silently into
    /// one that says so. The exception worth knowing about is a peer that is
    /// not this library, configured with a larger inbound limit; raise this
    /// field to match it.
    ///
    /// [`RepeError::MessageTooLarge`]: crate::RepeError::MessageTooLarge
    pub assumed_peer_frame_limit: Option<usize>,
}

impl Default for WebSocketLimits {
    fn default() -> Self {
        Self {
            max_incoming_frame_size: Some(DEFAULT_MAX_FRAME_SIZE),
            max_incoming_message_size: Some(DEFAULT_MAX_MESSAGE_SIZE),
            assumed_peer_frame_limit: Some(DEFAULT_MAX_FRAME_SIZE),
        }
    }
}

impl WebSocketLimits {
    /// Limits with every threshold removed.
    ///
    /// The outbound guard is off, so an undeliverable message again fails as a
    /// dropped connection rather than an error. Reserve this for a transport
    /// whose peer limits are known to be absent.
    pub fn unlimited() -> Self {
        Self {
            max_incoming_frame_size: None,
            max_incoming_message_size: None,
            assumed_peer_frame_limit: None,
        }
    }

    /// Set the largest inbound frame this endpoint accepts.
    pub fn with_max_incoming_frame_size(mut self, bytes: Option<usize>) -> Self {
        self.max_incoming_frame_size = bytes;
        self
    }

    /// Set the largest reassembled inbound message this endpoint accepts.
    pub fn with_max_incoming_message_size(mut self, bytes: Option<usize>) -> Self {
        self.max_incoming_message_size = bytes;
        self
    }

    /// Set the assumed peer frame limit used by the outbound guard.
    pub fn with_assumed_peer_frame_limit(mut self, bytes: Option<usize>) -> Self {
        self.assumed_peer_frame_limit = bytes;
        self
    }

    /// The outbound guard: `Err` when `size` exceeds the assumed peer limit.
    pub(crate) fn check_outbound(&self, size: usize) -> Result<(), crate::RepeError> {
        match self.assumed_peer_frame_limit {
            Some(limit) if size > limit => Err(crate::RepeError::MessageTooLarge { size, limit }),
            _ => Ok(()),
        }
    }
}

#[cfg(all(feature = "websocket", not(target_arch = "wasm32")))]
impl From<WebSocketLimits> for tokio_tungstenite::tungstenite::protocol::WebSocketConfig {
    fn from(limits: WebSocketLimits) -> Self {
        // Only the read-side thresholds map through. The outbound guard has no
        // transport equivalent, which is precisely why repe implements it.
        Self {
            max_frame_size: limits.max_incoming_frame_size,
            max_message_size: limits.max_incoming_message_size,
            ..Self::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_outbound_guard_matches_an_unconfigured_peers_inbound_limit() {
        let limits = WebSocketLimits::default();
        assert!(limits.check_outbound(DEFAULT_MAX_FRAME_SIZE).is_ok());
        let err = limits
            .check_outbound(DEFAULT_MAX_FRAME_SIZE + 1)
            .expect_err("one byte over the limit must be refused");
        assert!(matches!(
            err,
            crate::RepeError::MessageTooLarge { size, limit }
                if size == DEFAULT_MAX_FRAME_SIZE + 1 && limit == DEFAULT_MAX_FRAME_SIZE
        ));
    }

    #[test]
    fn removing_the_assumed_limit_disables_the_outbound_guard() {
        let limits = WebSocketLimits::default().with_assumed_peer_frame_limit(None);
        assert!(limits.check_outbound(usize::MAX).is_ok());
        assert!(
            WebSocketLimits::unlimited()
                .check_outbound(usize::MAX)
                .is_ok()
        );
    }

    /// The error names the configured assumption, since that is the number the
    /// caller can act on; the peer's real threshold is not observable.
    #[test]
    fn the_error_reports_both_the_size_and_the_assumed_limit() {
        let err = WebSocketLimits::default()
            .with_assumed_peer_frame_limit(Some(1024))
            .check_outbound(4096)
            .expect_err("over the limit");
        let text = err.to_string();
        assert!(text.contains("4096"), "{text}");
        assert!(text.contains("1024"), "{text}");
    }

    #[cfg(all(feature = "websocket", not(target_arch = "wasm32")))]
    #[test]
    fn only_the_read_side_thresholds_map_onto_the_transport_config() {
        use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
        let cfg: WebSocketConfig = WebSocketLimits::default()
            .with_max_incoming_frame_size(Some(123))
            .with_max_incoming_message_size(Some(456))
            .into();
        assert_eq!(cfg.max_frame_size, Some(123));
        assert_eq!(cfg.max_message_size, Some(456));
        // Buffer tuning is left at the transport's defaults: repe does not
        // expose those knobs, and inconsistent values make the transport panic.
        assert_eq!(
            cfg.write_buffer_size,
            WebSocketConfig::default().write_buffer_size
        );
    }
}
