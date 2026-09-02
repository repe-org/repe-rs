//! A REPE plugin host: `dlopen` a plugin and drive it over the C ABI.
//!
//! The mirror of [`repe_plugin`](../repe_plugin/index.html). Build it with
//!
//! ```text
//! cargo build --release --features plugin-host --example plugin_host
//! ```
//!
//! and point it at any conforming plugin:
//!
//! ```text
//! target/release/examples/plugin_host target/release/examples/librepe_plugin.so
//! ```
//!
//! The plugin's implementation language does not come into it. This drives the
//! Rust plugin example and a C++ Glaze plugin with the same code and the same
//! expectations, which is what CI uses it for — see `interop/README.md`.
//!
//! What the host itself has to get right is in `repe::plugin::host`: the version
//! handshake before the metadata is read, the optional lifecycle symbols, and
//! copying each response before the plugin's borrow expires. What is left here
//! is the part that is genuinely the application's — deciding which plugin a
//! request belongs to, and what to do with the answer.
//!
//! Beyond the published surface it expects (`gain`, `channel`, `calibrate`,
//! `identify`, `reset`), every check below holds for any conforming plugin,
//! which is what lets one binary drive two implementations. Behavior that is
//! this crate's rather than the protocol's — the response echoing the request
//! query, say, which Glaze's registry does not do — is pinned in
//! `tests/plugin_host.rs` instead, where the plugin under test is known.

use std::process::ExitCode;

use repe::constants::{ErrorCode, QueryFormat};
use repe::message::{Message, MessageView};
use repe::plugin::host::{HostError, LoadOrigin, Plugin};
use repe::server::Router;

/// Failures seen so far. Every check runs, so one bad expectation reports all of
/// them rather than only the first.
struct Checks {
    failures: u32,
}

impl Checks {
    fn check(&mut self, ok: bool, what: &str) {
        println!("  {} {what}", if ok { "ok  " } else { "FAIL" });
        if !ok {
            self.failures += 1;
        }
    }
}

/// A read: a request with no body. REPE separates a read from a write at the
/// frame level, by whether a body is present, so nothing about the path says
/// which one this is.
fn read(query: &str, id: u64) -> Vec<u8> {
    Message::builder()
        .id(id)
        .query_str(query)
        .query_format(QueryFormat::JsonPointer)
        .build()
        .to_vec()
}

/// A write, or a method call with an argument. Same frame either way: what
/// distinguishes them is what the path names.
fn write<T: serde::Serialize>(query: &str, id: u64, value: &T) -> Vec<u8> {
    Message::builder()
        .id(id)
        .query_str(query)
        .query_format(QueryFormat::JsonPointer)
        .body_json(value)
                .build()
        .to_vec()
}

/// A notify: no response is produced, by protocol.
fn notify(query: &str, id: u64) -> Vec<u8> {
    Message::builder()
        .id(id)
        .notify(true)
        .query_str(query)
        .query_format(QueryFormat::JsonPointer)
        .build()
        .to_vec()
}

fn run(path: &str) -> Result<u32, HostError> {
    // SAFETY: loading a native library runs its initializers. This example is
    // handed a plugin path by whoever runs it, which is exactly the trust
    // decision a real host's operator makes when populating a plugin directory.
    let plugin = unsafe { Plugin::load(path) }?;

    let mut checks = Checks { failures: 0 };
    let root = plugin.root_path().to_string();

    println!("metadata");
    println!(
        "       name='{}' version='{}' root='{}' abi={} origin={:?}",
        plugin.name(),
        plugin.version(),
        root,
        plugin.interface_version(),
        plugin.load_origin()
    );
    // `load` refuses anything that fails these, so reaching here is the check.
    // Restated so the output says what was established rather than only what
    // was read.
    checks.check(root.starts_with('/'), "root_path is an absolute prefix");
    checks.check(!plugin.name().is_empty(), "the plugin reports a name");

    // A host routes with `Plugin::claims`. `MessageView` reads the query out of
    // a frame without decoding the body, which is what makes this cheap enough
    // to do on every request.
    println!("routing");
    {
        let frame = read(&format!("{root}/gain"), 1);
        let query = MessageView::from_slice(&frame)
            .expect("a frame this process just built parses")
            .query_str()
            .expect("the query is UTF-8")
            .to_string();
        checks.check(
            plugin.claims(&query),
            "the request query matches the plugin's claimed root",
        );
        checks.check(
            !plugin.claims(&format!("{root}_other/gain")),
            "a plugin whose root shares a prefix with this one is not claimed",
        );
    }

    println!("field read");
    {
        let response = plugin
            .call(&read(&format!("{root}/gain"), 1))?
            .expect("a non-notify read produces a response");
        let message = Message::from_slice(&response).expect("the response is a REPE frame");
        checks.check(
            message.json_body::<f64>().is_ok_and(|gain| gain == 1.0),
            "read /gain returns the constructed value",
        );
        checks.check(message.header.id == 1, "the response echoes the request id");
    }

    println!("field write");
    {
        let acknowledged = plugin.call(&write(&format!("{root}/channel"), 2, &6u32))?;
        checks.check(acknowledged.is_some(), "a write is acknowledged");

        let response = plugin
            .call(&read(&format!("{root}/channel"), 3))?
            .expect("a non-notify read produces a response");
        let message = Message::from_slice(&response).expect("the response is a REPE frame");
        checks.check(
            message.json_body::<u32>().is_ok_and(|channel| channel == 6),
            "the read observes what the write left behind",
        );
    }

    println!("method call");
    {
        let response = plugin
            .call(&write(&format!("{root}/calibrate"), 4, &8.0f64))?
            .expect("a non-notify call produces a response");
        let message = Message::from_slice(&response).expect("the response is a REPE frame");
        checks.check(
            message.json_body::<f64>().is_ok_and(|gain| gain == 4.0),
            "the method returns its computed result",
        );
    }

    println!("zero-argument method");
    {
        let response = plugin
            .call(&read(&format!("{root}/identify"), 5))?
            .expect("a non-notify call produces a response");
        let message = Message::from_slice(&response).expect("the response is a REPE frame");
        checks.check(
            message
                .json_body::<String>()
                .is_ok_and(|identity| identity.starts_with("instrument fw")),
            "the method result decodes as a string",
        );
    }

    println!("notify");
    {
        let response = plugin.call(&notify(&format!("{root}/reset"), 6))?;
        checks.check(
            response.is_none(),
            "a notify answers with size 0, which the host reads as `send nothing`",
        );
    }

    println!("unknown method");
    {
        let response = plugin
            .call(&read(&format!("{root}/absent"), 7))?
            .expect("an unknown method is answered, not dropped");
        let message = Message::from_slice(&response).expect("the response is a REPE frame");
        checks.check(
            message.error_code() == Some(ErrorCode::MethodNotFound),
            "an unknown method is method_not_found, not a dropped frame",
        );
    }

    println!("malformed frame");
    {
        // Not a REPE frame at all. The plugin owes an answer anyway: a host that
        // received nothing would have no way to tell a broken plugin from a
        // notify.
        let response = plugin
            .call(b"not a repe frame at all")?
            .expect("a malformed frame is answered, not silently dropped");
        let message = Message::from_slice(&response).expect("the answer is still a REPE frame");
        checks.check(
            message.is_error(),
            "a malformed frame is answered with an error",
        );
        checks.check(
            message.header.id == 0,
            "the id is 0, because it could not be read",
        );
    }

    println!("mounted on a router");
    {
        // The other way to host a plugin: hand it to `Router::with_fallback` and
        // let the router resolve its own routes first. `Plugin` implements
        // `HandlerErased`, so the mount is one line and the frame marshalling is
        // the crate's rather than the application's.
        //
        // A second `Plugin::load` of the same path reaches the same instance —
        // `dlopen` refcounts by path and nothing is ever unloaded — so this
        // shares state with the handle above rather than starting a new plugin.
        //
        // SAFETY: as above; the library is already resident.
        let mounted = unsafe { Plugin::load(path) }?;
        // And it says so. A host that reloads a path it was handed reports this
        // rather than a success, since nothing was read and nothing changed.
        checks.check(
            mounted.load_origin() == LoadOrigin::AlreadyResident,
            "a second load of one path reports itself as already resident",
        );
        // `_blocking`: a plugin call enters a library this process did not
        // build, for a time nothing here bounds, so on the WebSocket server it
        // belongs off the reader task. On the TCP servers the two are the same.
        let router = Router::new()
            .with_typed("/host/ping", |_| Ok(serde_json::json!("pong")))
            .with_fallback_blocking(std::sync::Arc::new(mounted));

        let frame = router
            .call(&write("/host/ping", 8, &serde_json::Value::Null))
            .expect("the host route answers");
        let message = Message::from_slice(&frame).expect("the response is a REPE frame");
        checks.check(
            message
                .json_body::<String>()
                .is_ok_and(|pong| pong == "pong"),
            "a route the host owns is not shadowed by the fallback",
        );

        let frame = router
            .call(&read(&format!("{root}/channel"), 9))
            .expect("the plugin answers through the router");
        let message = Message::from_slice(&frame).expect("the response is a REPE frame");
        checks.check(
            message.json_body::<u32>().is_ok_and(|channel| channel == 6),
            "a request under the plugin's root reaches the plugin",
        );
        checks.check(message.header.id == 9, "the mounted response keeps the id");

        let frame = router
            .call(&read("/not/claimed/by/anyone", 10))
            .expect("a declined path is answered rather than dropped");
        let message = Message::from_slice(&frame).expect("the response is a REPE frame");
        checks.check(
            message.error_code() == Some(ErrorCode::MethodNotFound),
            "a path outside the plugin's root is method_not_found",
        );

        // A notify must reach the plugin, not merely produce no response: the
        // dispatch layer discards a notify's response whatever the handler did,
        // so `is_none()` on its own would pass even if the plugin were skipped.
        // `calibrate` moves gain off its reset value; the notify puts it back.
        router.call(&write(&format!("{root}/calibrate"), 11, &8.0f64));
        checks.check(
            router.call(&notify(&format!("{root}/reset"), 12)).is_none(),
            "notify semantics carry through the mount",
        );
        let frame = router
            .call(&read(&format!("{root}/gain"), 13))
            .expect("answered");
        let message = Message::from_slice(&frame).expect("the response is a REPE frame");
        checks.check(
            message.json_body::<f64>().is_ok_and(|gain| gain == 1.0),
            "the notify reached the plugin and reset its gain",
        );
    }

    println!("shutdown");
    {
        // Last, because it is visible to everything above it. The handle is
        // consumed, so the borrow checker enforces what `plugin.h` states: no
        // further calls are made after this.
        //
        // A second `Plugin::load` of this same path would reach this same
        // shut-down instance — the library is never unloaded and `dlopen`
        // refcounts by path — so a host that shuts a plugin down has retired it
        // for the life of the process.
        plugin.shutdown();
    }

    Ok(checks.failures)
}

fn main() -> ExitCode {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: plugin_host <path-to-plugin-library>");
        return ExitCode::from(2);
    };

    match run(&path) {
        Ok(0) => {
            println!("\nplugin host: PASS");
            ExitCode::SUCCESS
        }
        Ok(failures) => {
            println!("\nplugin host: FAIL ({failures} check(s))");
            ExitCode::FAILURE
        }
        Err(error) => {
            eprintln!("plugin host: {error}");
            ExitCode::from(2)
        }
    }
}
