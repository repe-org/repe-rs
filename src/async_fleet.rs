use crate::async_client::AsyncClient;
use crate::error::RepeError;
use crate::fleet::{
    ConnectSummary, DisconnectSummary, FleetError, FleetOptions, HealthStatus, Node, NodeConfig,
    ReconnectSummary, RemoteResult,
};
use crate::message::Message;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{Mutex, RwLock};

const DEFAULT_HEALTH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

struct AsyncNodeState {
    config: NodeConfig,
    tags: HashSet<String>,
    client: Mutex<Option<AsyncClient>>,
}

impl AsyncNodeState {
    fn new(config: NodeConfig) -> Self {
        Self {
            tags: config.tags.iter().cloned().collect(),
            config,
            client: Mutex::new(None),
        }
    }

    fn snapshot(&self) -> Node {
        Node {
            name: self.config.name.clone(),
            host: self.config.host.clone(),
            port: self.config.port,
            tags: self.tags.iter().cloned().collect(),
            timeout: self.config.timeout,
        }
    }

    fn timeout_or(&self, fallback: std::time::Duration) -> std::time::Duration {
        if self.config.timeout.is_zero() {
            fallback
        } else {
            self.config.timeout
        }
    }
}

#[derive(Clone)]
pub struct AsyncFleet {
    nodes: Arc<RwLock<HashMap<String, Arc<AsyncNodeState>>>>,
    options: FleetOptions,
}

impl Default for AsyncFleet {
    fn default() -> Self {
        Self {
            nodes: Arc::new(RwLock::new(HashMap::new())),
            options: FleetOptions::default(),
        }
    }
}

impl AsyncFleet {
    pub fn new(configs: Vec<NodeConfig>) -> Result<Self, FleetError> {
        Self::with_options(configs, FleetOptions::default())
    }

    pub fn with_options(
        configs: Vec<NodeConfig>,
        options: FleetOptions,
    ) -> Result<Self, FleetError> {
        if options.default_timeout.is_zero() {
            return Err(FleetError::InvalidTimeout);
        }
        if options.retry_policy.max_attempts < 1 {
            return Err(FleetError::InvalidRetryAttempts);
        }

        let mut names = HashSet::new();
        let mut nodes = HashMap::new();

        for config in configs {
            if config.host.is_empty() {
                return Err(FleetError::EmptyHost);
            }
            if config.name.is_empty() {
                return Err(FleetError::EmptyNodeName);
            }
            if config.port == 0 {
                return Err(FleetError::InvalidPort(config.port));
            }
            if config.timeout.is_zero() {
                return Err(FleetError::InvalidTimeout);
            }
            if !names.insert(config.name.clone()) {
                return Err(FleetError::DuplicateNodeName(config.name));
            }

            nodes.insert(config.name.clone(), Arc::new(AsyncNodeState::new(config)));
        }

        Ok(Self {
            nodes: Arc::new(RwLock::new(nodes)),
            options,
        })
    }

    pub fn options(&self) -> FleetOptions {
        self.options
    }

    pub async fn len(&self) -> usize {
        self.nodes.read().await.len()
    }

    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }

    pub async fn keys(&self) -> Vec<String> {
        self.nodes.read().await.keys().cloned().collect()
    }

    pub async fn node(&self, name: &str) -> Result<Node, FleetError> {
        let nodes = self.nodes.read().await;
        let node = nodes
            .get(name)
            .ok_or_else(|| FleetError::NodeNotFound(name.to_string()))?;
        Ok(node.snapshot())
    }

    pub async fn nodes(&self) -> Vec<Node> {
        self.nodes
            .read()
            .await
            .values()
            .map(|node| node.snapshot())
            .collect()
    }

    pub async fn connected_nodes(&self) -> Vec<Node> {
        let states = self.snapshot_node_states().await;
        let mut out = Vec::new();
        for state in states {
            if state.client.lock().await.is_some() {
                out.push(state.snapshot());
            }
        }
        out
    }

    pub async fn filter_nodes<T: AsRef<str>>(&self, tags: &[T]) -> Vec<Node> {
        let tag_set: HashSet<String> = tags.iter().map(|tag| tag.as_ref().to_string()).collect();
        self.nodes
            .read()
            .await
            .values()
            .filter(|node| tag_set.is_subset(&node.tags))
            .map(|node| node.snapshot())
            .collect()
    }

    pub async fn add_node(&self, config: NodeConfig) -> Result<(), FleetError> {
        if config.host.is_empty() {
            return Err(FleetError::EmptyHost);
        }
        if config.name.is_empty() {
            return Err(FleetError::EmptyNodeName);
        }
        if config.port == 0 {
            return Err(FleetError::InvalidPort(config.port));
        }
        if config.timeout.is_zero() {
            return Err(FleetError::InvalidTimeout);
        }

        let mut nodes = self.nodes.write().await;
        if nodes.contains_key(&config.name) {
            return Err(FleetError::DuplicateNodeName(config.name));
        }

        nodes.insert(config.name.clone(), Arc::new(AsyncNodeState::new(config)));
        Ok(())
    }

    pub async fn remove_node(&self, name: &str) -> bool {
        let removed = {
            let mut nodes = self.nodes.write().await;
            nodes.remove(name)
        };

        if let Some(state) = removed {
            let mut client = state.client.lock().await;
            *client = None;
            true
        } else {
            false
        }
    }

    pub async fn connect_all(&self) -> ConnectSummary {
        let states = self.snapshot_node_states().await;
        let mut tasks = Vec::with_capacity(states.len());

        for state in states {
            tasks.push(tokio::spawn(async move {
                let name = state.config.name.clone();
                let ok = ensure_connected(&state).await.is_ok();
                (name, ok)
            }));
        }

        let mut summary = ConnectSummary::default();
        for task in tasks {
            match task.await {
                Ok((name, true)) => summary.connected.push(name),
                Ok((name, false)) => summary.failed.push(name),
                Err(_) => summary.failed.push("<unknown>".to_string()),
            }
        }

        summary
    }

    pub async fn disconnect_all(&self) -> DisconnectSummary {
        let states = self.snapshot_node_states().await;
        let mut summary = DisconnectSummary::default();

        for state in states {
            let name = state.config.name.clone();
            let mut client = state.client.lock().await;
            *client = None;
            summary.disconnected.push(name);
        }

        summary
    }

    pub async fn reconnect_disconnected(&self) -> ReconnectSummary {
        let states = self.snapshot_node_states().await;
        let mut tasks = Vec::new();

        for state in states {
            if state.client.lock().await.is_none() {
                tasks.push(tokio::spawn(async move {
                    let name = state.config.name.clone();
                    let ok = ensure_connected(&state).await.is_ok();
                    (name, ok)
                }));
            }
        }

        let mut summary = ReconnectSummary::default();
        for task in tasks {
            match task.await {
                Ok((name, true)) => summary.reconnected.push(name),
                Ok((name, false)) => summary.failed.push(name),
                Err(_) => summary.failed.push("<unknown>".to_string()),
            }
        }

        summary
    }

    pub async fn is_connected_all(&self) -> bool {
        let states = self.snapshot_node_states().await;
        for state in states {
            if state.client.lock().await.is_none() {
                return false;
            }
        }
        true
    }

    pub async fn is_connected(&self, name: &str) -> Result<bool, FleetError> {
        let state = self.node_state(name).await?;
        Ok(state.client.lock().await.is_some())
    }

    pub async fn call_json(
        &self,
        node_name: &str,
        method: &str,
        params: Option<&Value>,
    ) -> Result<RemoteResult<Value>, FleetError> {
        let state = self.node_state(node_name).await?;
        Ok(self
            .call_json_with_retry(state, method.to_string(), params.cloned())
            .await)
    }

    pub async fn call_message(
        &self,
        node_name: &str,
        method: &str,
    ) -> Result<RemoteResult<Message>, FleetError> {
        let state = self.node_state(node_name).await?;
        Ok(self
            .call_message_with_retry(state, method.to_string())
            .await)
    }

    pub async fn broadcast_json<T: AsRef<str>>(
        &self,
        method: &str,
        params: Option<&Value>,
        tags: &[T],
    ) -> HashMap<String, RemoteResult<Value>> {
        let states = self.snapshot_target_nodes(tags).await;
        let mut tasks = Vec::with_capacity(states.len());

        for state in states {
            let fleet = self.clone();
            let method = method.to_string();
            let params = params.cloned();
            tasks.push(tokio::spawn(async move {
                let result = fleet.call_json_with_retry(state, method, params).await;
                (result.node.clone(), result)
            }));
        }

        let mut results = HashMap::new();
        for task in tasks {
            if let Ok((name, result)) = task.await {
                results.insert(name, result);
            }
        }

        results
    }

    pub async fn map_reduce_json<T: AsRef<str>, R, F>(
        &self,
        method: &str,
        params: Option<&Value>,
        tags: &[T],
        reduce_fn: F,
    ) -> R
    where
        F: FnOnce(Vec<RemoteResult<Value>>) -> R,
    {
        let results = self.broadcast_json(method, params, tags).await;
        reduce_fn(results.into_values().collect())
    }

    pub async fn health_check(&self, health_endpoint: &str) -> HashMap<String, HealthStatus> {
        let states = self.snapshot_node_states().await;
        let mut tasks = Vec::with_capacity(states.len());

        for state in states {
            let endpoint = health_endpoint.to_string();
            tasks.push(tokio::spawn(async move {
                let start = Instant::now();
                let name = state.config.name.clone();

                let outcome = async {
                    let client = ensure_connected(&state).await?;
                    client
                        .call_message_with_timeout(&endpoint, DEFAULT_HEALTH_TIMEOUT)
                        .await
                }
                .await;

                let status = match outcome {
                    Ok(_) => HealthStatus {
                        healthy: true,
                        latency: start.elapsed(),
                        error: None,
                    },
                    Err(err) => {
                        invalidate_client(&state).await;
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
        for task in tasks {
            if let Ok((name, status)) = task.await {
                results.insert(name, status);
            }
        }

        results
    }

    async fn snapshot_node_states(&self) -> Vec<Arc<AsyncNodeState>> {
        self.nodes.read().await.values().cloned().collect()
    }

    async fn snapshot_target_nodes<T: AsRef<str>>(&self, tags: &[T]) -> Vec<Arc<AsyncNodeState>> {
        let tag_set: HashSet<String> = tags.iter().map(|tag| tag.as_ref().to_string()).collect();
        self.nodes
            .read()
            .await
            .values()
            .filter(|node| tag_set.is_subset(&node.tags))
            .cloned()
            .collect()
    }

    async fn node_state(&self, name: &str) -> Result<Arc<AsyncNodeState>, FleetError> {
        self.nodes
            .read()
            .await
            .get(name)
            .cloned()
            .ok_or_else(|| FleetError::NodeNotFound(name.to_string()))
    }

    async fn call_json_with_retry(
        &self,
        state: Arc<AsyncNodeState>,
        method: String,
        params: Option<Value>,
    ) -> RemoteResult<Value> {
        let started = Instant::now();
        let mut last_error = None;

        for attempt in 0..self.options.retry_policy.max_attempts {
            let timeout = state.timeout_or(self.options.default_timeout);
            let call = async {
                let client = ensure_connected(&state).await?;
                if let Some(ref value) = params {
                    client.call_json_with_timeout(&method, value, timeout).await
                } else {
                    let message = client.call_message_with_timeout(&method, timeout).await?;
                    message.json_body::<Value>()
                }
            }
            .await;

            match call {
                Ok(value) => {
                    return RemoteResult {
                        node: state.config.name.clone(),
                        value: Some(value),
                        error: None,
                        elapsed: started.elapsed(),
                    };
                }
                Err(err) => {
                    let should_retry = is_retryable_error(&err);
                    last_error = Some(err);

                    if should_retry {
                        invalidate_client(&state).await;
                        if attempt + 1 < self.options.retry_policy.max_attempts {
                            tokio::time::sleep(self.options.retry_policy.delay).await;
                        }
                    } else {
                        break;
                    }
                }
            }
        }

        RemoteResult {
            node: state.config.name.clone(),
            value: None,
            error: last_error,
            elapsed: started.elapsed(),
        }
    }

    async fn call_message_with_retry(
        &self,
        state: Arc<AsyncNodeState>,
        method: String,
    ) -> RemoteResult<Message> {
        let started = Instant::now();
        let mut last_error = None;

        for attempt in 0..self.options.retry_policy.max_attempts {
            let timeout = state.timeout_or(self.options.default_timeout);
            let call = async {
                let client = ensure_connected(&state).await?;
                client.call_message_with_timeout(&method, timeout).await
            }
            .await;

            match call {
                Ok(value) => {
                    return RemoteResult {
                        node: state.config.name.clone(),
                        value: Some(value),
                        error: None,
                        elapsed: started.elapsed(),
                    };
                }
                Err(err) => {
                    let should_retry = is_retryable_error(&err);
                    last_error = Some(err);

                    if should_retry {
                        invalidate_client(&state).await;
                        if attempt + 1 < self.options.retry_policy.max_attempts {
                            tokio::time::sleep(self.options.retry_policy.delay).await;
                        }
                    } else {
                        break;
                    }
                }
            }
        }

        RemoteResult {
            node: state.config.name.clone(),
            value: None,
            error: last_error,
            elapsed: started.elapsed(),
        }
    }
}

async fn ensure_connected(state: &Arc<AsyncNodeState>) -> Result<AsyncClient, RepeError> {
    let mut client = state.client.lock().await;
    if let Some(existing) = client.as_ref() {
        return Ok(existing.clone());
    }

    let host = state.config.host.clone();
    let created = AsyncClient::connect((host.as_str(), state.config.port)).await?;
    let cloned = created.clone();
    *client = Some(created);
    Ok(cloned)
}

async fn invalidate_client(state: &Arc<AsyncNodeState>) {
    let mut client = state.client.lock().await;
    *client = None;
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
