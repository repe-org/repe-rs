use crate::error::RepeError;
use crate::fleet::{DEFAULT_BODY_FORMAT_CODE, DEFAULT_QUERY_FORMAT_CODE, build_message_for_udp};
use serde_json::Value;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use uniudp::fec::FecMode;
use uniudp::options::{SendIdentityOverrides, SendOptions};
use uniudp::sender::{SendFailure, SendRequest, Sender};

#[derive(Clone)]
pub struct UniUdpClient {
    inner: Arc<UniUdpClientInner>,
}

struct UniUdpClientInner {
    socket: Mutex<Option<UdpSocket>>,
    sender: Sender,
    destination: SocketAddr,
    next_repe_id: AtomicU64,
    redundancy: u16,
    chunk_size: u16,
    fec_group_size: u8,
    parity_shards: u8,
}

impl UniUdpClient {
    pub fn connect<A: ToSocketAddrs>(
        target: A,
        redundancy: usize,
        chunk_size: usize,
        fec_group_size: usize,
        parity_shards: usize,
    ) -> Result<Self, RepeError> {
        let destination = resolve_destination(target)?;
        let bind_addr = if destination.is_ipv4() {
            "0.0.0.0:0"
        } else {
            "[::]:0"
        };
        let socket = UdpSocket::bind(bind_addr)?;

        let redundancy = u16::try_from(redundancy).map_err(|_| {
            RepeError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "redundancy exceeds u16::MAX",
            ))
        })?;
        let chunk_size = u16::try_from(chunk_size).map_err(|_| {
            RepeError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "chunk_size exceeds u16::MAX",
            ))
        })?;
        let fec_group_size = u8::try_from(fec_group_size).map_err(|_| {
            RepeError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "fec_group_size exceeds u8::MAX",
            ))
        })?;
        if fec_group_size > 64 {
            return Err(RepeError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "fec_group_size exceeds UniUDP RS maximum (64)",
            )));
        }
        let parity_shards = u8::try_from(parity_shards).map_err(|_| {
            RepeError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "parity_shards exceeds u8::MAX",
            ))
        })?;
        if parity_shards == 0 || parity_shards > 16 {
            return Err(RepeError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "parity_shards must be between 1 and 16 for UniUDP RS FEC",
            )));
        }

        Ok(Self {
            inner: Arc::new(UniUdpClientInner {
                socket: Mutex::new(Some(socket)),
                sender: Sender::new(),
                destination,
                next_repe_id: AtomicU64::new(1),
                redundancy,
                chunk_size,
                fec_group_size,
                parity_shards,
            }),
        })
    }

    pub fn is_open(&self) -> bool {
        lock_socket(&self.inner.socket).is_some()
    }

    pub fn close(&self) {
        let mut socket = lock_socket(&self.inner.socket);
        *socket = None;
    }

    pub fn send_notify(&self, method: &str, params: Option<&Value>) -> Result<u64, RepeError> {
        self.send_with_formats(
            method,
            params,
            DEFAULT_QUERY_FORMAT_CODE,
            DEFAULT_BODY_FORMAT_CODE,
            true,
        )
    }

    pub fn send_request(&self, method: &str, params: Option<&Value>) -> Result<u64, RepeError> {
        self.send_with_formats(
            method,
            params,
            DEFAULT_QUERY_FORMAT_CODE,
            DEFAULT_BODY_FORMAT_CODE,
            false,
        )
    }

    pub fn send_with_formats(
        &self,
        method: &str,
        params: Option<&Value>,
        query_format: u16,
        body_format: u16,
        notify: bool,
    ) -> Result<u64, RepeError> {
        let socket = lock_socket(&self.inner.socket);
        let Some(socket) = socket.as_ref() else {
            return Err(RepeError::Io(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "udp client is closed",
            )));
        };

        let repe_id = self.next_repe_message_id();
        let message =
            build_message_for_udp(repe_id, method, params, query_format, body_format, notify)?;
        let payload = message.to_vec();
        let request = SendRequest::new(self.inner.destination, &payload)
            .with_options(self.send_options())
            .with_identity(SendIdentityOverrides::new().with_message_id(repe_id));

        let sent_key = self
            .inner
            .sender
            .send_with_socket(socket, request)
            .map_err(send_failure_to_repe_error)?;

        debug_assert_eq!(sent_key.message_id, repe_id);
        if sent_key.message_id != repe_id {
            return Err(RepeError::Io(std::io::Error::other(
                "uniudp returned a mismatched message id",
            )));
        }

        Ok(repe_id)
    }

    fn next_repe_message_id(&self) -> u64 {
        self.inner.next_repe_id.fetch_add(1, Ordering::Relaxed)
    }

    fn send_options(&self) -> SendOptions {
        let mut options = SendOptions::new()
            .with_redundancy(self.inner.redundancy)
            .with_chunk_size(self.inner.chunk_size);

        if self.inner.fec_group_size > 1 {
            options = options.with_fec_mode(FecMode::ReedSolomon {
                data_shards: self.inner.fec_group_size,
                parity_shards: self.inner.parity_shards,
            });
        }

        options
    }
}

fn resolve_destination<A: ToSocketAddrs>(target: A) -> Result<SocketAddr, RepeError> {
    let mut addrs = target.to_socket_addrs()?;
    addrs.next().ok_or_else(|| {
        RepeError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "target resolved to no socket addresses",
        ))
    })
}

fn send_failure_to_repe_error(err: SendFailure) -> RepeError {
    RepeError::Io(std::io::Error::other(format!("uniudp send failed: {err}")))
}

fn lock_socket(socket: &Mutex<Option<UdpSocket>>) -> MutexGuard<'_, Option<UdpSocket>> {
    match socket.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}
