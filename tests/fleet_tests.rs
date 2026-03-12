#![cfg(not(target_arch = "wasm32"))]

mod common;

use common::{
    TestServer, TransportFlakyServer, error_response_for, json_response_for, unused_port,
};
use repe::{
    ErrorCode, Fleet, FleetError, FleetOptions, NodeConfig, RemoteResult, RepeError, RetryPolicy,
};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

#[test]
fn fleet_construction_and_node_access() {
    let node1 = NodeConfig::new("127.0.0.1", 9001)
        .unwrap()
        .with_name("node-1")
        .unwrap()
        .with_tags(["compute", "primary"]);
    let node2 = NodeConfig::new("127.0.0.1", 9002)
        .unwrap()
        .with_name("node-2")
        .unwrap()
        .with_tags(["storage"]);

    let fleet = Fleet::new(vec![node1.clone(), node2.clone()]).unwrap();
    assert_eq!(fleet.len(), 2);
    assert!(!fleet.is_empty());

    let keys = fleet.keys();
    assert!(keys.contains(&"node-1".to_string()));
    assert!(keys.contains(&"node-2".to_string()));

    let node = fleet.node("node-1").unwrap();
    assert_eq!(node.name, "node-1");
    assert_eq!(node.port, 9001);

    let compute = fleet.filter_nodes(&["compute"]);
    assert_eq!(compute.len(), 1);
    assert_eq!(compute[0].name, "node-1");

    let all_nodes = fleet.nodes();
    assert_eq!(all_nodes.len(), 2);

    let duplicate = Fleet::new(vec![
        node1,
        node2.with_name("node-1").expect("rename node-2 -> node-1"),
    ]);
    assert!(matches!(duplicate, Err(FleetError::DuplicateNodeName(_))));
}

#[test]
fn fleet_dynamic_node_management() {
    let fleet = Fleet::default();
    assert_eq!(fleet.len(), 0);

    fleet
        .add_node(
            NodeConfig::new("127.0.0.1", 9101)
                .unwrap()
                .with_name("node-1")
                .unwrap(),
        )
        .unwrap();
    assert_eq!(fleet.len(), 1);

    fleet
        .add_node(
            NodeConfig::new("127.0.0.1", 9102)
                .unwrap()
                .with_name("node-2")
                .unwrap(),
        )
        .unwrap();
    assert_eq!(fleet.len(), 2);

    let duplicate = fleet.add_node(
        NodeConfig::new("127.0.0.1", 9103)
            .unwrap()
            .with_name("node-2")
            .unwrap(),
    );
    assert!(matches!(duplicate, Err(FleetError::DuplicateNodeName(_))));

    assert!(fleet.remove_node("node-1"));
    assert_eq!(fleet.len(), 1);
    assert!(!fleet.remove_node("missing"));
    assert_eq!(fleet.len(), 1);
}

#[test]
fn fleet_connection_invocation_health_and_retries() {
    let server1 = TestServer::spawn(Arc::new(|req| match req.query_utf8().as_str() {
        "/status" => json_response_for(req, &json!({"status": "ok", "node": 1})),
        "/compute" => {
            let value = req
                .json_body::<Value>()
                .ok()
                .and_then(|v| v.get("value").and_then(Value::as_i64))
                .unwrap_or(0);
            json_response_for(req, &json!({"result": value * 2, "node": 1}))
        }
        "/echo" => {
            let value = req.json_body::<Value>().unwrap_or_else(|_| json!({}));
            json_response_for(req, &value)
        }
        _ => error_response_for(req, ErrorCode::MethodNotFound, "unknown route"),
    }));

    let server2 = TestServer::spawn(Arc::new(|req| match req.query_utf8().as_str() {
        "/status" => json_response_for(req, &json!({"status": "ok", "node": 2})),
        "/compute" => {
            let value = req
                .json_body::<Value>()
                .ok()
                .and_then(|v| v.get("value").and_then(Value::as_i64))
                .unwrap_or(0);
            json_response_for(req, &json!({"result": value * 3, "node": 2}))
        }
        _ => error_response_for(req, ErrorCode::MethodNotFound, "unknown route"),
    }));

    let dead_port = unused_port();

    let options = FleetOptions {
        default_timeout: Duration::from_millis(400),
        retry_policy: RetryPolicy {
            max_attempts: 3,
            delay: Duration::from_millis(20),
        },
    };

    let configs = vec![
        NodeConfig::new("127.0.0.1", server1.addr().port())
            .unwrap()
            .with_name("server-1")
            .unwrap()
            .with_tags(["compute"]),
        NodeConfig::new("127.0.0.1", server2.addr().port())
            .unwrap()
            .with_name("server-2")
            .unwrap()
            .with_tags(["compute", "primary"]),
        NodeConfig::new("127.0.0.1", dead_port)
            .unwrap()
            .with_name("server-3")
            .unwrap(),
    ];

    let fleet = Fleet::with_options(configs, options).unwrap();

    let connected = fleet.connect_all();
    assert!(connected.connected.contains(&"server-1".to_string()));
    assert!(connected.connected.contains(&"server-2".to_string()));
    assert!(connected.failed.contains(&"server-3".to_string()));

    assert!(!fleet.is_connected_all());
    assert!(fleet.is_connected("server-1").unwrap());
    assert!(!fleet.is_connected("server-3").unwrap());

    let single = fleet
        .call_json("server-1", "/compute", Some(&json!({"value": 10})))
        .unwrap();
    assert!(single.succeeded());
    assert_eq!(single.value.as_ref().unwrap()["result"], 20);

    let missing = fleet.call_json("missing", "/status", None);
    assert!(matches!(missing, Err(FleetError::NodeNotFound(_))));

    let all_status = fleet.broadcast_json("/status", None, &[] as &[&str]);
    assert_eq!(all_status.len(), 3);
    assert!(all_status["server-1"].succeeded());
    assert!(all_status["server-2"].succeeded());
    assert!(all_status["server-3"].failed());

    let primary_only = fleet.broadcast_json("/status", None, &["primary"]);
    assert_eq!(primary_only.len(), 1);
    assert!(primary_only.contains_key("server-2"));

    let total = fleet.map_reduce_json(
        "/compute",
        Some(&json!({"value": 10})),
        &["compute"],
        |results| {
            results
                .into_iter()
                .filter_map(|result| {
                    if !result.succeeded() {
                        return None;
                    }
                    result
                        .value
                        .and_then(|value| value.get("result").and_then(Value::as_i64))
                })
                .sum::<i64>()
        },
    );
    assert_eq!(total, 50);

    let health = fleet.health_check("/status");
    assert_eq!(health.len(), 3);
    assert!(health["server-1"].healthy);
    assert!(health["server-2"].healthy);
    assert!(!health["server-3"].healthy);

    let disconnected = fleet.disconnect_all();
    assert!(disconnected.disconnected.contains(&"server-1".to_string()));
    assert!(disconnected.disconnected.contains(&"server-2".to_string()));
    assert!(disconnected.disconnected.contains(&"server-3".to_string()));

    let reconnected = fleet.reconnect_disconnected();
    assert!(reconnected.reconnected.contains(&"server-1".to_string()));
    assert!(reconnected.reconnected.contains(&"server-2".to_string()));
    assert!(reconnected.failed.contains(&"server-3".to_string()));

    let fleet = Arc::new(fleet);
    let mut workers = Vec::new();
    for i in 0..10 {
        let fleet = Arc::clone(&fleet);
        workers.push(thread::spawn(move || {
            fleet.broadcast_json("/echo", Some(&json!({"id": i})), &[] as &[&str])
        }));
    }

    for worker in workers {
        let results = worker.join().unwrap();
        assert_eq!(results.len(), 3);
        assert!(results["server-1"].succeeded());
        assert!(results["server-2"].failed());
        assert!(results["server-3"].failed());
    }
}

#[test]
fn fleet_retry_policy_recovers_from_transport_errors() {
    let (flaky_server, attempts) = TransportFlakyServer::spawn(2);

    let fleet = Fleet::with_options(
        vec![
            NodeConfig::new("127.0.0.1", flaky_server.addr().port())
                .unwrap()
                .with_name("flaky")
                .unwrap(),
        ],
        FleetOptions {
            default_timeout: Duration::from_millis(300),
            retry_policy: RetryPolicy {
                max_attempts: 5,
                delay: Duration::from_millis(10),
            },
        },
    )
    .unwrap();

    let connected = fleet.connect_all();
    assert_eq!(connected.connected, vec!["flaky".to_string()]);

    let result = fleet
        .call_json("flaky", "/flaky", Some(&json!({})))
        .unwrap();
    assert!(result.succeeded());
    let payload = result.value.as_ref().unwrap();
    assert_eq!(payload["success"], true);
    assert!(payload["attempt"].as_u64().unwrap() >= 3);
    assert!(attempts.load(Ordering::SeqCst) >= 3);
}

#[test]
fn fleet_retry_policy_does_not_retry_application_errors() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_handler = Arc::clone(&attempts);

    let server = TestServer::spawn(Arc::new(move |req| {
        if req.query_utf8() != "/flaky" {
            return error_response_for(req, ErrorCode::MethodNotFound, "unknown route");
        }

        attempts_handler.fetch_add(1, Ordering::SeqCst);
        error_response_for(req, ErrorCode::ApplicationErrorBase, "temporary failure")
    }));

    let fleet = Fleet::with_options(
        vec![
            NodeConfig::new("127.0.0.1", server.addr().port())
                .unwrap()
                .with_name("flaky")
                .unwrap(),
        ],
        FleetOptions {
            default_timeout: Duration::from_millis(300),
            retry_policy: RetryPolicy {
                max_attempts: 5,
                delay: Duration::from_millis(10),
            },
        },
    )
    .unwrap();

    let connected = fleet.connect_all();
    assert_eq!(connected.connected, vec!["flaky".to_string()]);

    let result = fleet
        .call_json("flaky", "/flaky", Some(&json!({})))
        .unwrap();
    assert!(result.failed());
    assert!(matches!(
        result.error.as_ref(),
        Some(RepeError::ServerError {
            code: ErrorCode::ApplicationErrorBase,
            ..
        })
    ));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
}

#[test]
fn remote_result_into_result_behaves_like_result() {
    let ok_result = RemoteResult {
        node: "node-1".to_string(),
        value: Some(json!({"ok": true})),
        error: None,
        elapsed: Duration::from_millis(1),
    };
    assert_eq!(ok_result.into_result().unwrap()["ok"], true);

    let err_result: RemoteResult<Value> = RemoteResult {
        node: "node-1".to_string(),
        value: None,
        error: Some(RepeError::Io(std::io::Error::other("failed"))),
        elapsed: Duration::from_millis(1),
    };
    assert!(err_result.into_result().is_err());
}
