use repe::{AsyncClient, Message, QueryFormat, RepeError, read_message, write_message};
use serde_json::{Value, json};
use std::io::{BufReader, BufWriter, Write};
use std::net::TcpListener;
use std::thread;
use std::time::Duration;

fn json_response_for(req: &Message, body: &Value) -> Message {
    Message::builder()
        .id(req.header.id)
        .query_bytes(req.query.clone())
        .query_format(
            QueryFormat::try_from(req.header.query_format).unwrap_or(QueryFormat::RawBinary),
        )
        .body_json(body)
        .expect("json body")
        .build()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_client_multiplexes_out_of_order_responses() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut writer = BufWriter::new(stream);

        let req_a = read_message(&mut reader).unwrap();
        let req_b = read_message(&mut reader).unwrap();

        let resp_b = json_response_for(
            &req_b,
            &json!({"path": req_b.query_utf8(), "id": req_b.header.id}),
        );
        let resp_a = json_response_for(
            &req_a,
            &json!({"path": req_a.query_utf8(), "id": req_a.header.id}),
        );

        write_message(&mut writer, &resp_b).unwrap();
        writer.flush().unwrap();
        write_message(&mut writer, &resp_a).unwrap();
        writer.flush().unwrap();
    });

    let client = AsyncClient::connect(addr).await.unwrap();

    let c1 = client.clone();
    let call_a = tokio::spawn(async move {
        let out = c1.call_json("/first", &json!({"v": 1})).await?;
        if out["path"] != "/first" {
            return Err(RepeError::Io(std::io::Error::other(
                "unexpected response for /first",
            )));
        }
        Ok::<(), RepeError>(())
    });

    let c2 = client.clone();
    let call_b = tokio::spawn(async move {
        let out = c2.call_json("/second", &json!({"v": 2})).await?;
        if out["path"] != "/second" {
            return Err(RepeError::Io(std::io::Error::other(
                "unexpected response for /second",
            )));
        }
        Ok::<(), RepeError>(())
    });

    call_a.await.unwrap().unwrap();
    call_b.await.unwrap().unwrap();

    tokio::task::spawn_blocking(move || server.join().unwrap())
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_client_per_request_timeout() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream);
        let _ = read_message(&mut reader).unwrap();
        thread::sleep(Duration::from_millis(200));
    });

    let client = AsyncClient::connect(addr).await.unwrap();
    let err = client
        .call_json_with_timeout("/slow", &json!({}), Duration::from_millis(50))
        .await
        .unwrap_err();

    match err {
        RepeError::Io(io_err) => assert_eq!(io_err.kind(), std::io::ErrorKind::TimedOut),
        other => panic!("unexpected error: {other:?}"),
    }

    tokio::task::spawn_blocking(move || server.join().unwrap())
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_client_batch_json_preserves_order() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut writer = BufWriter::new(stream);

        let mut requests = Vec::new();
        for _ in 0..3 {
            requests.push(read_message(&mut reader).unwrap());
        }

        for req in requests.into_iter().rev() {
            let response = json_response_for(&req, &json!({"path": req.query_utf8()}));
            write_message(&mut writer, &response).unwrap();
            writer.flush().unwrap();
        }
    });

    let client = AsyncClient::connect(addr).await.unwrap();
    let results = client
        .batch_json(vec![
            ("/a".to_string(), json!({"n": 1})),
            ("/b".to_string(), json!({"n": 2})),
            ("/c".to_string(), json!({"n": 3})),
        ])
        .await;

    assert_eq!(results.len(), 3);
    assert_eq!(results[0].as_ref().unwrap()["path"], "/a");
    assert_eq!(results[1].as_ref().unwrap()["path"], "/b");
    assert_eq!(results[2].as_ref().unwrap()["path"], "/c");

    tokio::task::spawn_blocking(move || server.join().unwrap())
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_client_ignores_late_response_for_timed_out_request() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut writer = BufWriter::new(stream);

        let req_1 = read_message(&mut reader).unwrap();
        let req_2 = read_message(&mut reader).unwrap();
        let (timed_out_req, pending_req) = if req_1.query_utf8() == "/timed_out" {
            (req_1, req_2)
        } else {
            (req_2, req_1)
        };

        thread::sleep(Duration::from_millis(120));

        let late_response =
            json_response_for(&timed_out_req, &json!({"path": timed_out_req.query_utf8()}));
        write_message(&mut writer, &late_response).unwrap();
        writer.flush().unwrap();

        let pending_response =
            json_response_for(&pending_req, &json!({"path": pending_req.query_utf8()}));
        write_message(&mut writer, &pending_response).unwrap();
        writer.flush().unwrap();
    });

    let client = AsyncClient::connect(addr).await.unwrap();

    let timed_client = client.clone();
    let timed_call = tokio::spawn(async move {
        timed_client
            .call_json_with_timeout("/timed_out", &json!({"n": 1}), Duration::from_millis(50))
            .await
    });

    let pending_client = client.clone();
    let pending_call = tokio::spawn(async move {
        pending_client
            .call_json_with_timeout(
                "/still_pending",
                &json!({"n": 2}),
                Duration::from_millis(500),
            )
            .await
    });

    let timed_err = timed_call.await.unwrap().unwrap_err();
    match timed_err {
        RepeError::Io(io_err) => assert_eq!(io_err.kind(), std::io::ErrorKind::TimedOut),
        other => panic!("unexpected timeout error: {other:?}"),
    }

    let pending_out = pending_call.await.unwrap().unwrap();
    assert_eq!(pending_out["path"], "/still_pending");

    tokio::task::spawn_blocking(move || server.join().unwrap())
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_client_unrecognized_response_id_is_discarded() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut req = read_message(&mut reader).unwrap();
        req.header.id += 1; // mangle ID so client won't match it

        let mut writer = BufWriter::new(stream);
        write_message(&mut writer, &req).unwrap();
        writer.flush().unwrap();
        // Server closes after a short delay; client should get a connection error
        thread::sleep(Duration::from_millis(200));
    });

    let client = AsyncClient::connect(addr).await.unwrap();
    let result = tokio::time::timeout(Duration::from_millis(2000), async {
        client.call_json("/x", &json!({"a": 1})).await
    })
    .await
    .expect("call_json should fail when server closes connection");

    // The unrecognized response is discarded; the caller fails when the
    // server closes the connection (not from a protocol-violation teardown).
    assert!(result.is_err());

    tokio::task::spawn_blocking(move || server.join().unwrap())
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_client_preserves_structured_fatal_response_loop_error() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let req = read_message(&mut reader).unwrap();

        let mut resp = Message::builder()
            .id(req.header.id)
            .query_bytes(req.query.clone())
            .query_format(QueryFormat::try_from(req.header.query_format).unwrap())
            .error_code(repe::ErrorCode::Ok)
            .body_json(&json!({"ok": true}))
            .unwrap()
            .build();
        resp.header.spec = 0;

        let mut writer = BufWriter::new(stream);
        write_message(&mut writer, &resp).unwrap();
        writer.flush().unwrap();
    });

    let client = AsyncClient::connect(addr).await.unwrap();
    let err = client
        .call_json("/fatal", &json!({"a": 1}))
        .await
        .unwrap_err();
    match err {
        RepeError::InvalidSpec(0) => {}
        other => panic!("unexpected error: {other:?}"),
    }

    tokio::task::spawn_blocking(move || server.join().unwrap())
        .await
        .unwrap();
}
