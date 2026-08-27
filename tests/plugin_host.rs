//! The host against a real shared library.
//!
//! `src/plugin/host.rs`'s own tests drive [`Plugin`] against in-process function
//! pointers, which is the only way to present a plugin that misbehaves in one
//! specific way. What they cannot cover is the half that only exists on disk:
//! `dlopen`, `dlsym`, and a response that crosses a genuine library boundary
//! rather than a call within this binary.
//!
//! So this builds `examples/repe_plugin.rs` as a `cdylib` and loads it — the
//! same artifact CI hands to the C++ Glaze host, driven here by the Rust one.
//!
//! Everything runs in a single test, in order, because `dlopen` is process-wide:
//! the library is loaded once and never unloaded, so a shutdown issued by one
//! test would be visible to every other one whatever order they ran in.

#![cfg(all(feature = "plugin-host", not(target_arch = "wasm32")))]

use std::path::{Path, PathBuf};
use std::process::Command;

use repe::constants::{ErrorCode, QueryFormat};
use repe::message::Message;
use repe::plugin::host::{HostError, Plugin};

/// Build the plugin example as a `cdylib` and return the artifact's path.
///
/// The nested `cargo` is what makes this test self-contained: the example is
/// declared `crate-type = ["cdylib"]`, so an ordinary `cargo test` never
/// produces it, and a test that silently skipped when it was absent would report
/// success having checked nothing.
fn plugin_library() -> PathBuf {
    // `<target>/<profile>/deps/<this test binary>`, so two levels up is the
    // profile directory and three is the target directory. Deriving both from
    // the running binary rather than assuming `target/debug` is what keeps this
    // working under `--release`, a custom `CARGO_TARGET_DIR`, or a workspace.
    let test_binary = std::env::current_exe().expect("the running test has a path");
    let profile_dir = test_binary
        .parent()
        .and_then(Path::parent)
        .expect("a test binary lives in <target>/<profile>/deps")
        .to_path_buf();
    let target_dir = profile_dir
        .parent()
        .expect("<profile> lives in <target>")
        .to_path_buf();
    let profile = profile_dir
        .file_name()
        .and_then(|name| name.to_str())
        .expect("the profile directory is named");

    let mut cargo = Command::new(env!("CARGO"));
    cargo
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .args(["build", "--features", "plugin", "--example", "repe_plugin"])
        .arg("--target-dir")
        .arg(&target_dir);
    // Cargo's profile directory is named `debug` for the `dev` profile and after
    // the profile itself otherwise.
    match profile {
        "debug" => {}
        "release" => {
            cargo.arg("--release");
        }
        custom => {
            cargo.args(["--profile", custom]);
        }
    }

    let status = cargo
        .status()
        .expect("cargo is on PATH inside a cargo test");
    assert!(status.success(), "building the plugin example failed");

    let library = profile_dir.join("examples").join(format!(
        "{}repe_plugin{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    ));
    assert!(
        library.is_file(),
        "the plugin example built but left no library at {}",
        library.display()
    );
    library
}

fn read(query: &str, id: u64) -> Vec<u8> {
    Message::builder()
        .id(id)
        .query_str(query)
        .query_format(QueryFormat::JsonPointer)
        .build()
        .to_vec()
}

fn write<T: serde::Serialize>(query: &str, id: u64, value: &T) -> Vec<u8> {
    Message::builder()
        .id(id)
        .query_str(query)
        .query_format(QueryFormat::JsonPointer)
        .body_json(value)
        .unwrap()
        .build()
        .to_vec()
}

fn parse(response: &[u8]) -> Message {
    Message::from_slice(response).expect("the plugin answers with a REPE frame")
}

#[test]
fn a_real_shared_library_loads_and_serves() {
    let path = plugin_library();

    // SAFETY: the library under test is this repository's own plugin example,
    // built by the call above.
    let plugin = unsafe { Plugin::load(&path) }.expect("the example is a conforming plugin");

    // --- what crossed the boundary at load time --------------------------
    assert_eq!(plugin.root_path(), "/instrument");
    assert_eq!(plugin.interface_version(), 3);
    // The macro defaults these to the manifest's own name and version, so the
    // plugin's identity cannot drift from the crate that built it.
    assert_eq!(plugin.name(), env!("CARGO_PKG_NAME"));
    assert_eq!(plugin.version(), env!("CARGO_PKG_VERSION"));
    assert_eq!(plugin.path(), path);

    // --- a read ------------------------------------------------------------
    let response = plugin
        .call(&read("/instrument/gain", 1))
        .expect("the call crosses the ABI")
        .expect("a non-notify read produces a response");
    let message = parse(&response);
    assert_eq!(message.error_code(), Some(ErrorCode::Ok));
    assert_eq!(message.json_body::<f64>().unwrap(), 1.0);
    assert_eq!(message.header.id, 1, "the response echoes the request id");
    assert_eq!(message.query_utf8(), "/instrument/gain");

    // --- a write, then a read that observes it -----------------------------
    assert!(
        plugin
            .call(&write("/instrument/channel", 2, &6u32))
            .unwrap()
            .is_some(),
        "a write is acknowledged"
    );
    let response = plugin
        .call(&read("/instrument/channel", 3))
        .unwrap()
        .unwrap();
    assert_eq!(parse(&response).json_body::<u32>().unwrap(), 6);

    // --- a method that computes --------------------------------------------
    let response = plugin
        .call(&write("/instrument/calibrate", 4, &8.0f64))
        .unwrap()
        .unwrap();
    assert_eq!(parse(&response).json_body::<f64>().unwrap(), 4.0);

    // --- a handler `Err` is the caller's error, not the host's -------------
    let response = plugin
        .call(&write("/instrument/calibrate", 5, &-1.0f64))
        .unwrap()
        .expect("a failing handler still answers");
    let message = parse(&response);
    assert!(message.is_error());
    assert!(
        message
            .error_message_utf8()
            .is_some_and(|text| text.contains("instrument fault")),
        "the error frame carries the handler's own message"
    );

    // --- a BEVE typed body over the boundary -------------------------------
    // `#[repe(typed)]` answers with a bulk BEVE array while everything above is
    // JSON, so this is the one exchange where the copy the host makes is of a
    // binary body rather than text.
    let response = plugin
        .call(&read("/instrument/samples", 6))
        .unwrap()
        .unwrap();
    let message = parse(&response);
    assert_eq!(message.decode_typed_slice::<f64>().unwrap().len(), 8);

    // --- notify: nothing comes back ----------------------------------------
    let notify = Message::builder()
        .id(7)
        .notify(true)
        .query_str("/instrument/reset")
        .query_format(QueryFormat::JsonPointer)
        .build()
        .to_vec();
    assert!(
        plugin.call(&notify).unwrap().is_none(),
        "a notify answers with size 0, which is `send nothing` rather than an error"
    );

    // --- an unknown method under the plugin's root -------------------------
    let response = plugin
        .call(&read("/instrument/absent", 8))
        .unwrap()
        .unwrap();
    assert_eq!(
        parse(&response).error_code(),
        Some(ErrorCode::MethodNotFound)
    );

    // --- a frame that is not a frame ---------------------------------------
    let response = plugin
        .call(b"not a repe frame at all")
        .unwrap()
        .expect("a malformed frame is answered, not dropped");
    let message = parse(&response);
    assert!(message.is_error());
    assert_eq!(message.header.id, 0, "the id could not be read");

    // --- a second handle reaches the same instance -------------------------
    // `dlopen` refcounts by path and nothing is ever unloaded, so this is the
    // same plugin, not a fresh one. It observes the write made through the
    // first handle, and its `repe_plugin_init` reports ALREADY_INITIALIZED,
    // which the host accepts rather than treating as a failure.
    //
    // SAFETY: as above — the same library, already resident.
    let second = unsafe { Plugin::load(&path) }.expect("a second load is not a failure");
    let response = second
        .call(&read("/instrument/channel", 9))
        .unwrap()
        .unwrap();
    assert_eq!(
        parse(&response).json_body::<u32>().unwrap(),
        6,
        "both handles drive one instance, so the state is shared"
    );

    // --- shutdown, last, because the whole process sees it -----------------
    plugin.shutdown();

    // `second` is still a live handle to the instance that was just shut down,
    // which is the situation `shutdown` consuming `self` cannot prevent: the
    // latch belongs to the library, not to any handle. A call through it is
    // refused with an answer rather than ignored, so the host has something to
    // report.
    let response = second
        .call(&read("/instrument/gain", 10))
        .unwrap()
        .expect("a call after shutdown is answered, not dropped");
    assert!(parse(&response).is_error());

    // And loading the path afresh does not recover it. The library is never
    // unloaded, so this reaches the same shut-down instance, whose initializer
    // now refuses — the plugin is retired for the life of the process. That is
    // why dropping a `Plugin` deliberately does not shut it down.
    //
    // SAFETY: as above.
    let error = unsafe { Plugin::load(&path) }
        .expect_err("a shut-down plugin cannot be brought back by reloading it");
    assert!(
        matches!(error, HostError::InitFailed { code, .. } if code == 1),
        "the refusal comes from the plugin's own initializer, got {error}"
    );
}

#[test]
fn a_file_that_is_not_a_library_is_refused_by_the_loader() {
    let not_a_library = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");

    // SAFETY: the platform loader is asked to load a file that is not a library.
    // It rejects it before running anything, which is the outcome under test.
    let error =
        unsafe { Plugin::load(&not_a_library) }.expect_err("the dynamic loader refuses a manifest");
    assert!(
        matches!(error, HostError::Load { ref path, .. } if path == &not_a_library),
        "the error names the path that failed, got {error}"
    );
}
