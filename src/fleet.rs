use crate::client::Client;
use crate::error::RepeError;
use crate::message::Message;
use serde_json::Value;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt::{Display, Formatter};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::thread;
use std::time::{Duration, Instant};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_RETRY_DELAY: Duration = Duration::from_secs(1);
const DEFAULT_HEALTH_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub enum FleetError {
    #[error("host cannot be empty")]
    EmptyHost,
    #[error("node name cannot be empty")]
    EmptyNodeName,
    #[error("port must be between 1 and 65535, got {0}")]
    InvalidPort(u16),
    #[error("timeout must be greater than zero")]
    InvalidTimeout,
    #[error("retry max_attempts must be at least 1")]
    InvalidRetryAttempts,
    #[error("duplicate node name: \"{0}\"")]
    DuplicateNodeName(String),
    #[error("node not found: \"{0}\"")]
    NodeNotFound(String),
    #[error("node initialization failed: {0}")]
    NodeInitialization(String),
    #[error("redundancy must be at least 1")]
    InvalidRedundancy,
    #[error("chunk_size must be at least 1")]
    InvalidChunkSize,
    #[error("fec_group_size must be at least 1")]
    InvalidFecGroupSize,
    #[error("parity_shards must be between 1 and 16")]
    InvalidParityShards,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeConfig {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub tags: BTreeSet<String>,
    pub timeout: Duration,
}

impl NodeConfig {
    pub fn new(host: impl Into<String>, port: u16) -> Result<Self, FleetError> {
        let host = host.into();
        validate_host(&host)?;
        validate_port(port)?;

        Ok(Self {
            name: host.clone(),
            host,
            port,
            tags: BTreeSet::new(),
            timeout: DEFAULT_TIMEOUT,
        })
    }

    pub fn with_name(mut self, name: impl Into<String>) -> Result<Self, FleetError> {
        let name = name.into();
        if name.is_empty() {
            return Err(FleetError::EmptyNodeName);
        }
        self.name = name;
        Ok(self)
    }

    pub fn with_tags(mut self, tags: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.tags = tags.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Result<Self, FleetError> {
        validate_timeout(timeout)?;
        self.timeout = timeout;
        Ok(self)
    }

    pub fn address(&self) -> (&str, u16) {
        (&self.host, self.port)
    }
}

impl Display for NodeConfig {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "NodeConfig(\"{}\", {}; name=\"{}\", tags={:?}, timeout={}ms)",
            self.host,
            self.port,
            self.name,
            self.tags,
            self.timeout.as_millis()
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    pub max_attempts: usize,
    pub delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            delay: DEFAULT_RETRY_DELAY,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FleetOptions {
    pub default_timeout: Duration,
    pub retry_policy: RetryPolicy,
}

impl Default for FleetOptions {
    fn default() -> Self {
        Self {
            default_timeout: DEFAULT_TIMEOUT,
            retry_policy: RetryPolicy::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub tags: BTreeSet<String>,
    pub timeout: Duration,
}

impl Node {
    pub fn address(&self) -> (&str, u16) {
        (&self.host, self.port)
    }
}

impl Display for Node {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "Node(\"{}\", {}:{})", self.name, self.host, self.port)
    }
}

#[derive(Debug)]
pub struct RemoteResult<T> {
    pub node: String,
    pub value: Option<T>,
    pub error: Option<RepeError>,
    pub elapsed: Duration,
}

impl<T> RemoteResult<T> {
    pub fn succeeded(&self) -> bool {
        self.error.is_none()
    }

    pub fn failed(&self) -> bool {
        self.error.is_some()
    }

    pub fn into_result(self) -> Result<T, RepeError> {
        match (self.value, self.error) {
            (Some(value), None) => Ok(value),
            (_, Some(err)) => Err(err),
            (None, None) => Err(RepeError::Io(std::io::Error::other(
                "remote result missing value and error",
            ))),
        }
    }
}

impl<T> Display for RemoteResult<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        if self.succeeded() {
            write!(
                f,
                "RemoteResult({}, success, {}ms)",
                self.node,
                self.elapsed.as_millis()
            )
        } else {
            write!(
                f,
                "RemoteResult({}, failed, {}ms)",
                self.node,
                self.elapsed.as_millis()
            )
        }
    }
}

#[derive(Debug)]
pub struct HealthStatus {
    pub healthy: bool,
    pub latency: Duration,
    pub error: Option<RepeError>,
}

#[derive(Debug, Default)]
pub struct ConnectSummary {
    pub connected: Vec<String>,
    pub failed: Vec<String>,
}

#[derive(Debug, Default)]
pub struct DisconnectSummary {
    pub disconnected: Vec<String>,
    pub failed: Vec<String>,
}

#[derive(Debug, Default)]
pub struct ReconnectSummary {
    pub reconnected: Vec<String>,
    pub failed: Vec<String>,
}

struct NodeState {
    config: NodeConfig,
    tags: BTreeSet<String>,
    client: Mutex<Option<Client>>,
}

impl NodeState {
    fn new(config: NodeConfig) -> Self {
        Self {
            tags: config.tags.clone(),
            config,
            client: Mutex::new(None),
        }
    }

    fn snapshot(&self) -> Node {
        Node {
            name: self.config.name.clone(),
            host: self.config.host.clone(),
            port: self.config.port,
            tags: self.tags.clone(),
            timeout: self.config.timeout,
        }
    }
}

#[derive(Clone)]
pub struct Fleet {
    nodes: Arc<RwLock<HashMap<String, Arc<NodeState>>>>,
    options: FleetOptions,
}

impl Default for Fleet {
    fn default() -> Self {
        Self {
            nodes: Arc::new(RwLock::new(HashMap::new())),
            options: FleetOptions::default(),
        }
    }
}

impl Fleet {
    pub fn new(configs: Vec<NodeConfig>) -> Result<Self, FleetError> {
        Self::with_options(configs, FleetOptions::default())
    }

    pub fn with_options(
        configs: Vec<NodeConfig>,
        options: FleetOptions,
    ) -> Result<Self, FleetError> {
        validate_fleet_options(&options)?;

        let mut names = HashSet::new();
        let mut nodes = HashMap::new();
        for config in configs {
            validate_node_config(&config)?;
            if !names.insert(config.name.clone()) {
                return Err(FleetError::DuplicateNodeName(config.name));
            }
            nodes.insert(config.name.clone(), Arc::new(NodeState::new(config)));
        }

        Ok(Self {
            nodes: Arc::new(RwLock::new(nodes)),
            options,
        })
    }

    pub fn options(&self) -> FleetOptions {
        self.options
    }

    pub fn len(&self) -> usize {
        read_nodes(&self.nodes).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn keys(&self) -> Vec<String> {
        read_nodes(&self.nodes).keys().cloned().collect()
    }

    pub fn node(&self, name: &str) -> Result<Node, FleetError> {
        let nodes = read_nodes(&self.nodes);
        let node = nodes
            .get(name)
            .ok_or_else(|| FleetError::NodeNotFound(name.to_string()))?;
        Ok(node.snapshot())
    }

    pub fn nodes(&self) -> Vec<Node> {
        read_nodes(&self.nodes)
            .values()
            .map(|node| node.snapshot())
            .collect()
    }

    pub fn connected_nodes(&self) -> Vec<Node> {
        read_nodes(&self.nodes)
            .values()
            .filter_map(|node| {
                if lock_node_client(&node.client).is_some() {
                    Some(node.snapshot())
                } else {
                    None
                }
            })
            .collect()
    }

    /// Returns nodes that contain all provided tags.
    ///
    /// Passing an empty `tags` slice matches all nodes.
    pub fn filter_nodes<T: AsRef<str>>(&self, tags: &[T]) -> Vec<Node> {
        let tag_set: BTreeSet<String> = tags.iter().map(|tag| tag.as_ref().to_string()).collect();
        read_nodes(&self.nodes)
            .values()
            .filter(|node| tag_set.is_subset(&node.tags))
            .map(|node| node.snapshot())
            .collect()
    }

    pub fn add_node(&self, config: NodeConfig) -> Result<(), FleetError> {
        validate_node_config(&config)?;
        let mut nodes = write_nodes(&self.nodes);
        if nodes.contains_key(&config.name) {
            return Err(FleetError::DuplicateNodeName(config.name));
        }
        nodes.insert(config.name.clone(), Arc::new(NodeState::new(config)));
        Ok(())
    }

    pub fn remove_node(&self, name: &str) -> bool {
        let mut nodes = write_nodes(&self.nodes);
        if let Some(node) = nodes.remove(name) {
            let mut client = lock_node_client(&node.client);
            *client = None;
            true
        } else {
            false
        }
    }

    pub fn connect_all(&self) -> ConnectSummary {
        let nodes = self.snapshot_node_states();
        let mut handles = Vec::with_capacity(nodes.len());

        for node in nodes {
            handles.push(thread::spawn(move || {
                let name = node.config.name.clone();
                let connected = ensure_connected(&node).is_ok();
                (name, connected)
            }));
        }

        let mut summary = ConnectSummary::default();
        for handle in handles {
            match handle.join() {
                Ok((name, true)) => summary.connected.push(name),
                Ok((name, false)) => summary.failed.push(name),
                Err(_) => summary.failed.push("<unknown>".to_string()),
            }
        }

        summary
    }

    pub fn disconnect_all(&self) -> DisconnectSummary {
        let nodes = self.snapshot_node_states();
        let mut summary = DisconnectSummary::default();

        for node in nodes {
            let name = node.config.name.clone();
            let mut client = lock_node_client(&node.client);
            *client = None;
            summary.disconnected.push(name);
        }

        summary
    }

    pub fn reconnect_disconnected(&self) -> ReconnectSummary {
        let nodes = self.snapshot_node_states();
        let mut handles = Vec::new();

        for node in nodes {
            if lock_node_client(&node.client).is_none() {
                handles.push(thread::spawn(move || {
                    let name = node.config.name.clone();
                    let connected = ensure_connected(&node).is_ok();
                    (name, connected)
                }));
            }
        }

        let mut summary = ReconnectSummary::default();
        for handle in handles {
            match handle.join() {
                Ok((name, true)) => summary.reconnected.push(name),
                Ok((name, false)) => summary.failed.push(name),
                Err(_) => summary.failed.push("<unknown>".to_string()),
            }
        }

        summary
    }

    pub fn is_connected_all(&self) -> bool {
        read_nodes(&self.nodes)
            .values()
            .all(|node| lock_node_client(&node.client).is_some())
    }

    pub fn is_connected(&self, name: &str) -> Result<bool, FleetError> {
        let nodes = read_nodes(&self.nodes);
        let node = nodes
            .get(name)
            .ok_or_else(|| FleetError::NodeNotFound(name.to_string()))?;
        Ok(lock_node_client(&node.client).is_some())
    }

    pub fn call_json(
        &self,
        node_name: &str,
        method: &str,
        params: Option<&Value>,
    ) -> Result<RemoteResult<Value>, FleetError> {
        let node = self.node_state(node_name)?;
        Ok(self.call_json_with_retry(node, method.to_string(), params.cloned()))
    }

    pub fn call_message(
        &self,
        node_name: &str,
        method: &str,
    ) -> Result<RemoteResult<Message>, FleetError> {
        let node = self.node_state(node_name)?;
        Ok(self.call_message_with_retry(node, method.to_string()))
    }

    /// Broadcasts to nodes matching all provided tags.
    ///
    /// Passing an empty `tags` slice matches all nodes.
    pub fn broadcast_json<T: AsRef<str>>(
        &self,
        method: &str,
        params: Option<&Value>,
        tags: &[T],
    ) -> HashMap<String, RemoteResult<Value>> {
        let target_nodes = self.snapshot_target_nodes(tags);
        let mut handles = Vec::with_capacity(target_nodes.len());

        for node in target_nodes {
            let fleet = self.clone();
            let method = method.to_string();
            let params = params.cloned();
            handles.push(thread::spawn(move || {
                let result = fleet.call_json_with_retry(node, method, params);
                (result.node.clone(), result)
            }));
        }

        let mut results = HashMap::new();
        for handle in handles {
            if let Ok((name, result)) = handle.join() {
                results.insert(name, result);
            }
        }

        results
    }

    /// Broadcasts and reduces over nodes matching all provided tags.
    ///
    /// Passing an empty `tags` slice matches all nodes.
    pub fn map_reduce_json<T: AsRef<str>, R, F>(
        &self,
        method: &str,
        params: Option<&Value>,
        tags: &[T],
        reduce_fn: F,
    ) -> R
    where
        F: FnOnce(Vec<RemoteResult<Value>>) -> R,
    {
        let results = self.broadcast_json(method, params, tags);
        reduce_fn(results.into_values().collect())
    }

    pub fn health_check(&self, health_endpoint: &str) -> HashMap<String, HealthStatus> {
        let target_nodes = self.snapshot_node_states();
        let mut handles = Vec::with_capacity(target_nodes.len());

        for node in target_nodes {
            let endpoint = health_endpoint.to_string();
            handles.push(thread::spawn(move || {
                let start = Instant::now();
                let name = node.config.name.clone();

                let outcome = (|| {
                    let client = ensure_connected(&node)?;
                    client.call_message_with_timeout(&endpoint, DEFAULT_HEALTH_TIMEOUT)
                })();

                let status = match outcome {
                    Ok(_) => HealthStatus {
                        healthy: true,
                        latency: start.elapsed(),
                        error: None,
                    },
                    Err(err) => {
                        invalidate_client(&node);
                        HealthStatus {
                            healthy: false,
                            latency: start.elapsed(),
                            error: Some(err),
                        }
                    }
                };

                (name, status)
            }));
        }

        let mut results = HashMap::new();
        for handle in handles {
            if let Ok((name, status)) = handle.join() {
                results.insert(name, status);
            }
        }

        results
    }

    fn snapshot_node_states(&self) -> Vec<Arc<NodeState>> {
        read_nodes(&self.nodes).values().cloned().collect()
    }

    fn snapshot_target_nodes<T: AsRef<str>>(&self, tags: &[T]) -> Vec<Arc<NodeState>> {
        let tag_set: BTreeSet<String> = tags.iter().map(|tag| tag.as_ref().to_string()).collect();
        read_nodes(&self.nodes)
            .values()
            .filter(|node| tag_set.is_subset(&node.tags))
            .cloned()
            .collect()
    }

    fn node_state(&self, name: &str) -> Result<Arc<NodeState>, FleetError> {
        read_nodes(&self.nodes)
            .get(name)
            .cloned()
            .ok_or_else(|| FleetError::NodeNotFound(name.to_string()))
    }

    fn call_json_with_retry(
        &self,
        node: Arc<NodeState>,
        method: String,
        params: Option<Value>,
    ) -> RemoteResult<Value> {
        let started = Instant::now();
        let mut last_error = None;

        for attempt in 0..self.options.retry_policy.max_attempts {
            let timeout = node.config.timeout;
            let call = (|| {
                let client = ensure_connected(&node)?;
                if let Some(ref value) = params {
                    client.call_json_with_timeout(&method, value, timeout)
                } else {
                    let message = client.call_message_with_timeout(&method, timeout)?;
                    message.json_body::<Value>()
                }
            })();

            match call {
                Ok(value) => {
                    return RemoteResult {
                        node: node.config.name.clone(),
                        value: Some(value),
                        error: None,
                        elapsed: started.elapsed(),
                    };
                }
                Err(err) => {
                    let should_retry = is_retryable_error(&err);
                    last_error = Some(err);

                    if should_retry {
                        invalidate_client(&node);
                        if attempt + 1 < self.options.retry_policy.max_attempts {
                            thread::sleep(self.options.retry_policy.delay);
                        }
                    } else {
                        break;
                    }
                }
            }
        }

        RemoteResult {
            node: node.config.name.clone(),
            value: None,
            error: last_error,
            elapsed: started.elapsed(),
        }
    }

    fn call_message_with_retry(
        &self,
        node: Arc<NodeState>,
        method: String,
    ) -> RemoteResult<Message> {
        let started = Instant::now();
        let mut last_error = None;

        for attempt in 0..self.options.retry_policy.max_attempts {
            let timeout = node.config.timeout;
            let call = (|| {
                let client = ensure_connected(&node)?;
                client.call_message_with_timeout(&method, timeout)
            })();

            match call {
                Ok(value) => {
                    return RemoteResult {
                        node: node.config.name.clone(),
                        value: Some(value),
                        error: None,
                        elapsed: started.elapsed(),
                    };
                }
                Err(err) => {
                    let should_retry = is_retryable_error(&err);
                    last_error = Some(err);

                    if should_retry {
                        invalidate_client(&node);
                        if attempt + 1 < self.options.retry_policy.max_attempts {
                            thread::sleep(self.options.retry_policy.delay);
                        }
                    } else {
                        break;
                    }
                }
            }
        }

        RemoteResult {
            node: node.config.name.clone(),
            value: None,
            error: last_error,
            elapsed: started.elapsed(),
        }
    }
}

impl Display for Fleet {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let total = self.len();
        let connected = self.connected_nodes().len();
        write!(f, "Fleet({connected}/{total} nodes connected)")
    }
}

fn validate_host(host: &str) -> Result<(), FleetError> {
    if host.is_empty() {
        return Err(FleetError::EmptyHost);
    }
    Ok(())
}

fn validate_port(port: u16) -> Result<(), FleetError> {
    if port == 0 {
        return Err(FleetError::InvalidPort(port));
    }
    Ok(())
}

fn validate_timeout(timeout: Duration) -> Result<(), FleetError> {
    if timeout.is_zero() {
        return Err(FleetError::InvalidTimeout);
    }
    Ok(())
}

pub(crate) fn validate_node_config(config: &NodeConfig) -> Result<(), FleetError> {
    validate_host(&config.host)?;
    validate_port(config.port)?;
    validate_timeout(config.timeout)?;
    if config.name.is_empty() {
        return Err(FleetError::EmptyNodeName);
    }
    Ok(())
}

pub(crate) fn validate_fleet_options(options: &FleetOptions) -> Result<(), FleetError> {
    validate_timeout(options.default_timeout)?;
    if options.retry_policy.max_attempts < 1 {
        return Err(FleetError::InvalidRetryAttempts);
    }
    Ok(())
}

fn ensure_connected(node: &Arc<NodeState>) -> Result<Client, RepeError> {
    let mut client = lock_node_client(&node.client);
    if let Some(existing) = client.as_ref() {
        return Ok(existing.clone());
    }

    let created = Client::connect(node.config.address())?;
    let cloned = created.clone();
    *client = Some(created);
    Ok(cloned)
}

fn is_retryable_error(err: &RepeError) -> bool {
    match err {
        RepeError::Io(io_err) => matches!(
            io_err.kind(),
            std::io::ErrorKind::TimedOut
                | std::io::ErrorKind::ConnectionRefused
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::NotConnected
                | std::io::ErrorKind::UnexpectedEof
                | std::io::ErrorKind::WouldBlock
                | std::io::ErrorKind::Interrupted
        ),
        RepeError::ServerError { .. } => false,
        _ => false,
    }
}

fn invalidate_client(node: &Arc<NodeState>) {
    let mut client = lock_node_client(&node.client);
    *client = None;
}

fn lock_node_client(client: &Mutex<Option<Client>>) -> MutexGuard<'_, Option<Client>> {
    match client.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn read_nodes(
    nodes: &Arc<RwLock<HashMap<String, Arc<NodeState>>>>,
) -> std::sync::RwLockReadGuard<'_, HashMap<String, Arc<NodeState>>> {
    match nodes.read() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn write_nodes(
    nodes: &Arc<RwLock<HashMap<String, Arc<NodeState>>>>,
) -> std::sync::RwLockWriteGuard<'_, HashMap<String, Arc<NodeState>>> {
    match nodes.write() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}
