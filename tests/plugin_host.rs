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
use repe::plugin::host::{HostError, LoadOrigin, Plugin};
use repe::server::Router;
use std::sync::Arc;

/// Build the plugin example as a `cdylib` and return the artifact's path.
///
/// The nested `cargo` is what makes this test self-contained: the example is
/// declared `crate-type = ["cdylib"]`, so an ordinary `cargo test` never
/// produces it, and a test that silently skipped when it was absent would report
/// success having checked nothing.
///
/// The path comes from cargo's own JSON output rather than from arithmetic on
/// `current_exe`. Deriving it looks easy and is not: nightly cargo builds test
/// binaries into `<target>/debug/build/<pkg>/<hash>/out/`, where counting
/// parents finds a hash where a profile name should be. Asking cargo removes the
/// guess, and the profile with it — the plugin is built with the default one
/// whatever this test was compiled with, since all it has to do is load.
/// The part of a `cargo --message-format=json` line this test reads.
///
/// Declared rather than walked as a tree, and read under
/// [`repe::WirePolicy`]: cargo's messages carry a great many members this does
/// not name, and a strict read would refuse every one of them.
#[derive(Default)]
struct CargoMessage {
    reason: String,
    target: CargoTarget,
    filenames: Vec<String>,
}
structio::object!(CargoMessage {
    reason,
    target,
    filenames
});

#[derive(Default)]
struct CargoTarget {
    name: String,
}
structio::object!(CargoTarget { name });

fn plugin_library() -> PathBuf {
    let output = Command::new(env!("CARGO"))
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .args([
            "build",
            "--features",
            "plugin",
            "--example",
            "repe_plugin",
            "--message-format=json-render-diagnostics",
        ])
        .output()
        .expect("cargo is on PATH inside a cargo test");
    assert!(
        output.status.success(),
        "building the plugin example failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    // One `compiler-artifact` line per built target; the example's carries the
    // shared library among its `filenames`. Matching on the suffix rather than
    // taking the first is what keeps this right on a platform that emits an
    // import library beside the `.dll`.
    let library = String::from_utf8(output.stdout)
        .expect("cargo's JSON output is UTF-8")
        .lines()
        .filter_map(|line| {
            structio::json::from_str_with::<repe::WirePolicy, CargoMessage>(line).ok()
        })
        .filter(|message| message.reason == "compiler-artifact")
        .filter(|message| message.target.name == "repe_plugin")
        .filter_map(|message| {
            message
                .filenames
                .into_iter()
                .find(|name| name.ends_with(std::env::consts::DLL_SUFFIX))
                .map(PathBuf::from)
        })
        .next_back()
        .expect("the plugin example reports a shared library among its artifacts");

    assert!(
        library.is_file(),
        "cargo reported an artifact that is not there: {}",
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
    assert_eq!(
        plugin.load_origin(),
        LoadOrigin::Mapped,
        "nothing in this binary links the plugin, so this load mapped it"
    );
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
    assert_eq!(
        second.load_origin(),
        LoadOrigin::AlreadyResident,
        "the second load reached the resident copy; a host that reports it as a \
         successful reload is reporting a no-op"
    );
    let response = second
        .call(&read("/instrument/channel", 9))
        .unwrap()
        .unwrap();
    assert_eq!(
        parse(&response).json_body::<u32>().unwrap(),
        6,
        "both handles drive one instance, so the state is shared"
    );

    // --- mounted on a router, which is what `with_fallback` is for ---------
    // The seam the two halves meet at: the router resolves its own routes
    // first and hands every miss to the plugin, which claims what is under
    // its root and declines the rest. Scoped so the extra handle goes away
    // before the shutdown below.
    {
        // SAFETY: as above — the same library, already resident.
        let mounted = unsafe { Plugin::load(&path) }.expect("already resident");
        assert_eq!(mounted.load_origin(), LoadOrigin::AlreadyResident);
        let router = Router::new()
            .with_typed("/local", |_: Option<i64>| {
                Ok("served by the host".to_string())
            })
            .with_fallback(Arc::new(mounted));

        // A route the host owns is untouched by the fallback.
        let message = parse(&router.call(&write("/local", 11, &None::<i64>)).unwrap());
        assert_eq!(message.json_body::<String>().unwrap(), "served by the host");

        // Under the plugin's root, the plugin's own frame comes back whole —
        // body, id, and the query it echoed.
        let message = parse(&router.call(&read("/instrument/channel", 12)).unwrap());
        assert_eq!(message.json_body::<u32>().unwrap(), 6);
        assert_eq!(message.header.id, 12);
        assert_eq!(message.query_utf8(), "/instrument/channel");

        // A BEVE body survives the round trip through the router as well.
        let message = parse(&router.call(&read("/instrument/samples", 13)).unwrap());
        assert_eq!(message.decode_typed_slice::<f64>().unwrap().len(), 8);

        // Outside the plugin's root nobody claims the path, and the caller is
        // told so rather than left waiting.
        let message = parse(&router.call(&read("/elsewhere", 14)).unwrap());
        assert_eq!(message.error_code(), Some(ErrorCode::MethodNotFound));

        // A path the plugin claims but does not serve is refused by the plugin,
        // and that frame is forwarded rather than replaced. The two are told
        // apart by their text: the struct mounted at the plugin's root frames
        // its own miss, where the host's disclaim above says "Method not found".
        let frame = router
            .call(&read("/instrument/absent", 141))
            .expect("answered");
        let message = Message::from_slice(&frame).unwrap();
        assert_eq!(message.error_code(), Some(ErrorCode::MethodNotFound));
        let text = message.error_message_utf8().unwrap();
        assert!(
            !text.starts_with("Method not found:"),
            "the plugin's own frame was replaced by the host's: {text:?}"
        );

        // Notify semantics carry through the mount: nothing is written back —
        // and the notify actually reaches the plugin, which the `is_none()`
        // alone cannot show, since dispatch discards a notify's response
        // whatever the handler did. So gain is moved off its reset value first,
        // and the notify is what puts it back.
        router
            .call(&write("/instrument/calibrate", 15, &8.0f64))
            .expect("answered");
        let notify = Message::builder()
            .id(16)
            .notify(true)
            .query_str("/instrument/reset")
            .query_format(QueryFormat::JsonPointer)
            .build()
            .to_vec();
        assert!(router.call(&notify).is_none());
        let frame = router
            .call(&read("/instrument/gain", 17))
            .expect("answered");
        assert_eq!(
            Message::from_slice(&frame)
                .unwrap()
                .json_body::<f64>()
                .unwrap(),
            1.0,
            "the notify reached the plugin and reset its gain"
        );
    }

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

/// The other half of [`Plugin::load_origin`]: it answers about *this path*,
/// not about the plugin, so a build published under a new path is a fresh load
/// even though an identical binary is already resident.
///
/// This one is safe to run beside the test above, unlike a second load of the
/// same path: a copy at a different path is a different image, with its own
/// statics and its own initializer, so nothing it does is visible to the
/// original mapping.
#[test]
fn a_copy_at_a_new_path_is_a_fresh_load() {
    let original = plugin_library();
    // Next to the original so any relative runtime search path still resolves,
    // and named for the test so a stale copy is recognizable in `target/`.
    let copy = original.with_file_name(format!(
        "repe_plugin_fresh_path_copy{}",
        std::env::consts::DLL_SUFFIX
    ));
    std::fs::copy(&original, &copy).expect("the target directory is writable");

    // SAFETY: a byte-for-byte copy of this repository's own plugin example.
    let plugin = unsafe { Plugin::load(&copy) }.expect("the copy is the same conforming plugin");
    assert_eq!(
        plugin.load_origin(),
        LoadOrigin::Mapped,
        "a path the loader has never seen is a fresh load, whatever is resident \
         under another name"
    );
    assert_eq!(plugin.root_path(), "/instrument");

    // And the second load of *that* path is resident, exactly as for the
    // original — which is what makes the flag about the path and not about the
    // file's contents.
    //
    // SAFETY: as above — the same library, now resident.
    let again = unsafe { Plugin::load(&copy) }.expect("a second load is not a failure");
    assert_eq!(again.load_origin(), LoadOrigin::AlreadyResident);
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
