use crate::fleet::FleetError;
use crate::udp_client::UniUdpClient;
use serde_json::Value;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt::{Display, Formatter};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UniUdpNodeConfig {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub tags: Vec<String>,
    pub redundancy: usize,
    pub chunk_size: usize,
    pub fec_group_size: usize,
    pub parity_shards: usize,
}

impl UniUdpNodeConfig {
    pub fn new(host: impl Into<String>, port: u16) -> Result<Self, FleetError> {
        let host = host.into();
        if host.is_empty() {
            return Err(FleetError::EmptyHost);
        }
        if port == 0 {
            return Err(FleetError::InvalidPort(port));
        }

        Ok(Self {
            name: host.clone(),
            host,
            port,
            tags: Vec::new(),
            redundancy: 1,
            chunk_size: 1024,
            fec_group_size: 4,
            parity_shards: 2,
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

    pub fn with_redundancy(mut self, redundancy: usize) -> Result<Self, FleetError> {
        if redundancy < 1 {
            return Err(FleetError::InvalidRedundancy);
        }
        self.redundancy = redundancy;
        Ok(self)
    }

    pub fn with_chunk_size(mut self, chunk_size: usize) -> Result<Self, FleetError> {
        if chunk_size < 1 {
            return Err(FleetError::InvalidChunkSize);
        }
        self.chunk_size = chunk_size;
        Ok(self)
    }

    pub fn with_fec_group_size(mut self, fec_group_size: usize) -> Result<Self, FleetError> {
        if fec_group_size < 1 {
            return Err(FleetError::InvalidFecGroupSize);
        }
        self.fec_group_size = fec_group_size;
        Ok(self)
    }

    pub fn with_parity_shards(mut self, parity_shards: usize) -> Result<Self, FleetError> {
        if !(1..=16).contains(&parity_shards) {
            return Err(FleetError::InvalidParityShards);
        }
        self.parity_shards = parity_shards;
        Ok(self)
    }
}

impl Display for UniUdpNodeConfig {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "UniUdpNodeConfig(\"{}\", {}; name=\"{}\", tags={:?}, redundancy={}, chunk_size={}, fec_group_size={}, parity_shards={})",
            self.host,
            self.port,
            self.name,
            self.tags,
            self.redundancy,
            self.chunk_size,
            self.fec_group_size,
            self.parity_shards
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UniUdpNode {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub tags: BTreeSet<String>,
    pub redundancy: usize,
    pub chunk_size: usize,
    pub fec_group_size: usize,
    pub parity_shards: usize,
    pub open: bool,
}

#[derive(Debug)]
pub struct SendResult {
    pub node: String,
    pub message_id: u64,
    pub error: Option<crate::error::RepeError>,
    pub elapsed: Duration,
}

impl SendResult {
    pub fn succeeded(&self) -> bool {
        self.error.is_none()
    }

    pub fn failed(&self) -> bool {
        self.error.is_some()
    }
}

impl Display for SendResult {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        if self.succeeded() {
            write!(
                f,
                "SendResult({}, msg_id={}, {}ms)",
                self.node,
                self.message_id,
                self.elapsed.as_millis()
            )
        } else {
            write!(
                f,
                "SendResult({}, failed, {}ms)",
                self.node,
                self.elapsed.as_millis()
            )
        }
    }
}

struct UniUdpNodeState {
    config: UniUdpNodeConfig,
    tags: HashSet<String>,
    client: UniUdpClient,
}

impl UniUdpNodeState {
    fn new(config: UniUdpNodeConfig) -> Result<Self, crate::error::RepeError> {
        let client = UniUdpClient::connect(
            (config.host.as_str(), config.port),
            config.redundancy,
            config.chunk_size,
            config.fec_group_size,
            config.parity_shards,
        )?;

        Ok(Self {
            tags: config.tags.iter().cloned().collect(),
            config,
            client,
        })
    }

    fn snapshot(&self) -> UniUdpNode {
        UniUdpNode {
            name: self.config.name.clone(),
            host: self.config.host.clone(),
            port: self.config.port,
            tags: self.tags.iter().cloned().collect(),
            redundancy: self.config.redundancy,
            chunk_size: self.config.chunk_size,
            fec_group_size: self.config.fec_group_size,
            parity_shards: self.config.parity_shards,
            open: self.client.is_open(),
        }
    }
}

#[derive(Clone)]
pub struct UniUdpFleet {
    nodes: Arc<RwLock<HashMap<String, Arc<UniUdpNodeState>>>>,
}

impl Default for UniUdpFleet {
    fn default() -> Self {
        Self {
            nodes: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

impl UniUdpFleet {
    pub fn new(configs: Vec<UniUdpNodeConfig>) -> Result<Self, FleetError> {
        let mut names = HashSet::new();
        let mut nodes = HashMap::new();

        for config in configs {
            validate_node_config(&config)?;
            if !names.insert(config.name.clone()) {
                return Err(FleetError::DuplicateNodeName(config.name));
            }

            let state = UniUdpNodeState::new(config)
                .map_err(|err| FleetError::NodeInitialization(err.to_string()))?;
            nodes.insert(state.config.name.clone(), Arc::new(state));
        }

        Ok(Self {
            nodes: Arc::new(RwLock::new(nodes)),
        })
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

    pub fn node(&self, name: &str) -> Result<UniUdpNode, FleetError> {
        let nodes = read_nodes(&self.nodes);
        let node = nodes
            .get(name)
            .ok_or_else(|| FleetError::NodeNotFound(name.to_string()))?;
        Ok(node.snapshot())
    }

    pub fn nodes(&self) -> Vec<UniUdpNode> {
        read_nodes(&self.nodes)
            .values()
            .map(|node| node.snapshot())
            .collect()
    }

    /// Returns nodes that contain all provided tags.
    ///
    /// Passing an empty `tags` slice matches all nodes.
    pub fn filter_nodes<T: AsRef<str>>(&self, tags: &[T]) -> Vec<UniUdpNode> {
        let tag_set: HashSet<String> = tags.iter().map(|tag| tag.as_ref().to_string()).collect();
        read_nodes(&self.nodes)
            .values()
            .filter(|node| tag_set.is_subset(&node.tags))
            .map(|node| node.snapshot())
            .collect()
    }

    pub fn add_node(&self, config: UniUdpNodeConfig) -> Result<(), FleetError> {
        validate_node_config(&config)?;

        let state = UniUdpNodeState::new(config)
            .map_err(|err| FleetError::NodeInitialization(err.to_string()))?;

        let mut nodes = write_nodes(&self.nodes);
        if nodes.contains_key(&state.config.name) {
            return Err(FleetError::DuplicateNodeName(state.config.name.clone()));
        }

        nodes.insert(state.config.name.clone(), Arc::new(state));
        Ok(())
    }

    pub fn remove_node(&self, name: &str) -> bool {
        let mut nodes = write_nodes(&self.nodes);
        if let Some(node) = nodes.remove(name) {
            node.client.close();
            true
        } else {
            false
        }
    }

    pub fn close(&self) {
        for node in read_nodes(&self.nodes).values() {
            node.client.close();
        }
    }

    /// Sends notify-style UDP messages to nodes matching all provided tags.
    ///
    /// Passing an empty `tags` slice matches all nodes.
    pub fn send_notify<T: AsRef<str>>(
        &self,
        method: &str,
        params: Option<&Value>,
        tags: &[T],
    ) -> HashMap<String, SendResult> {
        self.send_with_selection(method, params, tags, true)
    }

    /// Sends request-style UDP messages to nodes matching all provided tags.
    ///
    /// Passing an empty `tags` slice matches all nodes.
    pub fn send_request<T: AsRef<str>>(
        &self,
        method: &str,
        params: Option<&Value>,
        tags: &[T],
    ) -> HashMap<String, SendResult> {
        self.send_with_selection(method, params, tags, false)
    }

    /// Sends notify-style UDP messages to all nodes.
    pub fn notify_all(&self, method: &str, params: Option<&Value>) -> HashMap<String, SendResult> {
        self.send_notify(method, params, &[] as &[&str])
    }

    pub fn send_notify_to(
        &self,
        node_name: &str,
        method: &str,
        params: Option<&Value>,
    ) -> Result<SendResult, FleetError> {
        let node = self
            .node_state(node_name)
            .ok_or_else(|| FleetError::NodeNotFound(node_name.to_string()))?;
        Ok(send_to_node(
            node,
            method.to_string(),
            params.cloned(),
            true,
        ))
    }

    fn send_with_selection<T: AsRef<str>>(
        &self,
        method: &str,
        params: Option<&Value>,
        tags: &[T],
        notify: bool,
    ) -> HashMap<String, SendResult> {
        let target_nodes = self.snapshot_target_nodes(tags);
        let mut handles = Vec::with_capacity(target_nodes.len());

        for node in target_nodes {
            let method = method.to_string();
            let params = params.cloned();
            handles.push(thread::spawn(move || {
                let result = send_to_node(node, method, params, notify);
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

    fn snapshot_target_nodes<T: AsRef<str>>(&self, tags: &[T]) -> Vec<Arc<UniUdpNodeState>> {
        let tag_set: HashSet<String> = tags.iter().map(|tag| tag.as_ref().to_string()).collect();
        read_nodes(&self.nodes)
            .values()
            .filter(|node| tag_set.is_subset(&node.tags))
            .cloned()
            .collect()
    }

    fn node_state(&self, name: &str) -> Option<Arc<UniUdpNodeState>> {
        read_nodes(&self.nodes).get(name).cloned()
    }
}

impl Display for UniUdpFleet {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "UniUdpFleet({} nodes)", self.len())
    }
}

fn send_to_node(
    node: Arc<UniUdpNodeState>,
    method: String,
    params: Option<Value>,
    notify: bool,
) -> SendResult {
    let started = Instant::now();

    let operation = || {
        if notify {
            node.client.send_notify(&method, params.as_ref())
        } else {
            node.client.send_request(&method, params.as_ref())
        }
    };

    match operation() {
        Ok(message_id) => SendResult {
            node: node.config.name.clone(),
            message_id,
            error: None,
            elapsed: started.elapsed(),
        },
        Err(err) => SendResult {
            node: node.config.name.clone(),
            message_id: 0,
            error: Some(err),
            elapsed: started.elapsed(),
        },
    }
}

fn validate_node_config(config: &UniUdpNodeConfig) -> Result<(), FleetError> {
    if config.host.is_empty() {
        return Err(FleetError::EmptyHost);
    }
    if config.name.is_empty() {
        return Err(FleetError::EmptyNodeName);
    }
    if config.port == 0 {
        return Err(FleetError::InvalidPort(config.port));
    }
    if config.redundancy < 1 {
        return Err(FleetError::InvalidRedundancy);
    }
    if config.chunk_size < 1 {
        return Err(FleetError::InvalidChunkSize);
    }
    if config.fec_group_size < 1 {
        return Err(FleetError::InvalidFecGroupSize);
    }
    if !(1..=16).contains(&config.parity_shards) {
        return Err(FleetError::InvalidParityShards);
    }
    Ok(())
}

fn read_nodes(
    nodes: &Arc<RwLock<HashMap<String, Arc<UniUdpNodeState>>>>,
) -> std::sync::RwLockReadGuard<'_, HashMap<String, Arc<UniUdpNodeState>>> {
    match nodes.read() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn write_nodes(
    nodes: &Arc<RwLock<HashMap<String, Arc<UniUdpNodeState>>>>,
) -> std::sync::RwLockWriteGuard<'_, HashMap<String, Arc<UniUdpNodeState>>> {
    match nodes.write() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}
