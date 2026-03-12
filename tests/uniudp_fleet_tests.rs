#![cfg(all(not(target_arch = "wasm32"), feature = "fleet-udp"))]

use repe::{FleetError, Message, UniUdpFleet, UniUdpNodeConfig};
use serde_json::json;
use std::net::UdpSocket;
use std::time::Duration;
use uniudp::message::SourcePolicy;
use uniudp::options::ReceiveOptions;
use uniudp::receiver::Receiver;

fn recv_message(receiver: &mut Receiver, socket: &mut UdpSocket) -> Message {
    let report = receiver
        .receive_message(
            socket,
            ReceiveOptions::new()
                .with_source_policy(SourcePolicy::AnyFirstSource)
                .with_inactivity_timeout(Duration::from_millis(200))
                .with_overall_timeout(Duration::from_secs(1)),
        )
        .expect("receive uniudp message");
    let payload = report
        .try_materialize_complete()
        .expect("materialize complete payload");
    Message::from_slice(&payload).expect("decode repe payload")
}

#[test]
fn uniudp_fleet_construction_and_access() {
    let cfg1 = UniUdpNodeConfig::new("127.0.0.1", 5001)
        .unwrap()
        .with_name("node-1")
        .unwrap()
        .with_tags(["sensor", "primary"])
        .with_redundancy(2)
        .unwrap()
        .with_parity_shards(3)
        .unwrap();
    let cfg2 = UniUdpNodeConfig::new("127.0.0.1", 5002)
        .unwrap()
        .with_name("node-2")
        .unwrap()
        .with_tags(["gateway"]);

    let fleet = UniUdpFleet::new(vec![cfg1.clone(), cfg2.clone()]).unwrap();
    assert_eq!(fleet.len(), 2);

    let keys = fleet.keys();
    assert!(keys.contains(&"node-1".to_string()));
    assert!(keys.contains(&"node-2".to_string()));

    let node = fleet.node("node-1").unwrap();
    assert_eq!(node.name, "node-1");
    assert_eq!(node.redundancy, 2);
    assert_eq!(node.parity_shards, 3);

    let default_cfg = UniUdpNodeConfig::new("127.0.0.1", 5003).unwrap();
    assert_eq!(default_cfg.parity_shards, 2);
    assert!(matches!(
        default_cfg.clone().with_parity_shards(0),
        Err(FleetError::InvalidParityShards)
    ));
    assert!(matches!(
        default_cfg.with_parity_shards(17),
        Err(FleetError::InvalidParityShards)
    ));

    let filtered = fleet.filter_nodes(&["sensor", "primary"]);
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].name, "node-1");

    let duplicate = UniUdpFleet::new(vec![cfg1, cfg2.with_name("node-1").unwrap()]);
    assert!(matches!(duplicate, Err(FleetError::DuplicateNodeName(_))));
}

#[test]
fn uniudp_fleet_send_operations_and_filtering() {
    let mut server1 = UdpSocket::bind("127.0.0.1:0").expect("bind udp server1");
    server1
        .set_read_timeout(Some(Duration::from_millis(500)))
        .expect("set read timeout server1");
    let mut server2 = UdpSocket::bind("127.0.0.1:0").expect("bind udp server2");
    server2
        .set_read_timeout(Some(Duration::from_millis(500)))
        .expect("set read timeout server2");
    let mut rx1 = Receiver::new();
    let mut rx2 = Receiver::new();

    let fleet = UniUdpFleet::new(vec![
        UniUdpNodeConfig::new("127.0.0.1", server1.local_addr().unwrap().port())
            .unwrap()
            .with_name("server-1")
            .unwrap()
            .with_tags(["group-a"]),
        UniUdpNodeConfig::new("127.0.0.1", server2.local_addr().unwrap().port())
            .unwrap()
            .with_name("server-2")
            .unwrap()
            .with_tags(["group-b"]),
    ])
    .unwrap();

    let broadcast = fleet.send_notify("/ping", Some(&json!({"broadcast": true})), &[] as &[&str]);
    assert_eq!(broadcast.len(), 2);
    assert!(broadcast["server-1"].succeeded());
    assert!(broadcast["server-2"].succeeded());
    assert!(broadcast["server-1"].message_id > 0);
    assert!(broadcast["server-2"].message_id > 0);

    let msg1 = recv_message(&mut rx1, &mut server1);
    let msg2 = recv_message(&mut rx2, &mut server2);
    assert_eq!(msg1.query_utf8(), "/ping");
    assert_eq!(msg2.query_utf8(), "/ping");
    assert_eq!(
        msg1.json_body::<serde_json::Value>().unwrap()["broadcast"],
        true
    );
    assert_eq!(
        msg2.json_body::<serde_json::Value>().unwrap()["broadcast"],
        true
    );

    let only_a = fleet.send_notify("/group", Some(&json!({"name": "a"})), &["group-a"]);
    assert_eq!(only_a.len(), 1);
    assert!(only_a.contains_key("server-1"));
    assert!(only_a["server-1"].succeeded());

    let msg_a = recv_message(&mut rx1, &mut server1);
    assert_eq!(msg_a.query_utf8(), "/group");
    assert_eq!(msg_a.json_body::<serde_json::Value>().unwrap()["name"], "a");

    let filtered_miss = rx2.receive_message(
        &mut server2,
        ReceiveOptions::new()
            .with_source_policy(SourcePolicy::AnyFirstSource)
            .with_inactivity_timeout(Duration::from_millis(50))
            .with_overall_timeout(Duration::from_millis(50)),
    );
    assert!(filtered_miss.is_err());

    let notify_all = fleet.notify_all("/heartbeat", Some(&json!({"ok": true})));
    assert_eq!(notify_all.len(), 2);
    assert!(notify_all["server-1"].succeeded());
    assert!(notify_all["server-2"].succeeded());

    let hb1 = recv_message(&mut rx1, &mut server1);
    let hb2 = recv_message(&mut rx2, &mut server2);
    assert_eq!(hb1.query_utf8(), "/heartbeat");
    assert_eq!(hb2.query_utf8(), "/heartbeat");

    let request_results =
        fleet.send_request("/request", Some(&json!({"value": 7})), &[] as &[&str]);
    assert_eq!(request_results.len(), 2);
    assert!(request_results["server-1"].succeeded());
    assert!(request_results["server-2"].succeeded());

    let req1 = recv_message(&mut rx1, &mut server1);
    let req2 = recv_message(&mut rx2, &mut server2);
    assert_eq!(req1.query_utf8(), "/request");
    assert_eq!(req2.query_utf8(), "/request");
    assert_eq!(req1.header.notify, 0);
    assert_eq!(req2.header.notify, 0);

    let single = fleet
        .send_notify_to("server-1", "/single", Some(&json!({"id": 42})))
        .unwrap();
    assert!(single.succeeded());

    let single_msg = recv_message(&mut rx1, &mut server1);
    assert_eq!(single_msg.query_utf8(), "/single");
    assert_eq!(
        single_msg.json_body::<serde_json::Value>().unwrap()["id"],
        42
    );

    let missing = fleet.send_notify_to("missing", "/single", None);
    assert!(matches!(missing, Err(FleetError::NodeNotFound(_))));
}

#[test]
fn uniudp_fleet_dynamic_nodes_and_close() {
    let mut server = UdpSocket::bind("127.0.0.1:0").expect("bind udp server");
    server
        .set_read_timeout(Some(Duration::from_millis(500)))
        .expect("set read timeout");
    let mut rx = Receiver::new();

    let fleet = UniUdpFleet::default();
    fleet
        .add_node(
            UniUdpNodeConfig::new("127.0.0.1", server.local_addr().unwrap().port())
                .unwrap()
                .with_name("node-1")
                .unwrap(),
        )
        .unwrap();

    let duplicate = fleet.add_node(
        UniUdpNodeConfig::new("127.0.0.1", server.local_addr().unwrap().port())
            .unwrap()
            .with_name("node-1")
            .unwrap(),
    );
    assert!(matches!(duplicate, Err(FleetError::DuplicateNodeName(_))));

    let sent = fleet.send_notify("/before-close", None, &[] as &[&str]);
    assert!(sent["node-1"].succeeded());
    let msg = recv_message(&mut rx, &mut server);
    assert_eq!(msg.query_utf8(), "/before-close");

    fleet.close();

    let closed = fleet.send_notify("/after-close", None, &[] as &[&str]);
    assert!(closed["node-1"].failed());

    assert!(fleet.remove_node("node-1"));
    assert!(!fleet.remove_node("missing"));
}
