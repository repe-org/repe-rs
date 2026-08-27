//! The C-ABI plugin surface, driven the way a host drives it.
//!
//! Exercises the symbols `#[repe::plugin]` generates by calling them through
//! `extern "C"` declarations, so what is under test is the linkable ABI — the
//! symbol names, the signatures, and the buffer contract — rather than the Rust
//! functions behind it. The dispatch logic underneath has its own unit tests in
//! `src/plugin.rs`; what those cannot cover is whether the exports resolve.
//!
//! No `dlopen` happens here: these call the generated exports directly, in this
//! process. The loading half is `tests/plugin_host.rs`, which builds the plugin
//! example as a real `cdylib` and drives it through `repe::plugin::host`.

#![cfg(all(feature = "plugin", not(target_arch = "wasm32")))]

use std::ffi::{CStr, c_char};

use repe::constants::{ErrorCode, QueryFormat};
use repe::plugin::{REPE_PLUGIN_INTERFACE_VERSION, RepeBuffer, RepePluginData, RepeResult};
use repe::server::Router;
use repe::{Message, RepeStruct};
use serde::{Deserialize, Serialize};

#[derive(Default, Serialize, Deserialize, RepeStruct)]
#[repe(methods)]
struct Counter {
    value: i64,
}

#[repe::methods]
impl Counter {
    fn bump(&mut self, by: i64) -> i64 {
        self.value += by;
        self.value
    }
}

#[repe::plugin(name = "counter", version = "2.5.0", root = "/counter")]
fn build() -> Router {
    Router::new()
        .with_json("/counter/echo", Ok)
        .with_struct("/counter", Counter::default())
        .0
}

/// The symbols as a host sees them: resolved by name, with the signatures from
/// `glaze/rpc/repe/plugin.h`. Declaring them rather than calling the generated
/// items directly is the point — it fails to link if the macro emits the wrong
/// name or the wrong shape, which is the failure a `dlopen`ing host would hit.
///
/// In a child module because the generated definitions occupy these names in the
/// parent, and a module cannot both define and declare one name. The linker
/// resolves these to those same exports.
mod host_abi {
    use super::{RepeBuffer, RepePluginData, RepeResult, c_char};

    unsafe extern "C" {
        pub fn repe_plugin_interface_version() -> u32;
        pub fn repe_plugin_info() -> *const RepePluginData;
        pub fn repe_plugin_init() -> RepeResult;
        pub fn repe_plugin_shutdown();
        pub fn repe_plugin_call(request: *const c_char, request_size: u64) -> RepeBuffer;
    }
}

fn request(path: &str, body: &serde_json::Value, notify: bool) -> Vec<u8> {
    Message::builder()
        .id(4)
        .notify(notify)
        .query_str(path)
        .query_format(QueryFormat::JsonPointer)
        .body_json(body)
        .unwrap()
        .build()
        .to_vec()
}

/// Call across the ABI and copy the response, which is what the buffer contract
/// requires of a host: the bytes are valid only until this thread's next call.
fn host_call(frame: &[u8]) -> Option<Vec<u8>> {
    let buffer =
        unsafe { host_abi::repe_plugin_call(frame.as_ptr().cast::<c_char>(), frame.len() as u64) };
    assert!(
        !buffer.data.is_null(),
        "`data` is never null, so a host may build a string_view from it unconditionally"
    );
    if buffer.size == 0 {
        return None;
    }
    Some(unsafe {
        std::slice::from_raw_parts(buffer.data.cast::<u8>(), buffer.size as usize).to_vec()
    })
}

/// One test, in order, because the plugin ABI is a process-global singleton:
/// `repe_plugin_shutdown` is observable by every other call in the binary, so
/// the lifecycle cannot be split across tests that the harness runs in parallel.
#[test]
fn the_plugin_abi_behaves_as_a_host_expects() {
    // --- version handshake, before anything reads the metadata struct -------
    assert_eq!(
        unsafe { host_abi::repe_plugin_interface_version() },
        REPE_PLUGIN_INTERFACE_VERSION
    );
    // --- metadata ----------------------------------------------------------
    let info = unsafe { &*host_abi::repe_plugin_info() };
    let cstr = |ptr: *const c_char| unsafe { CStr::from_ptr(ptr) }.to_str().unwrap().to_string();
    assert_eq!(cstr(info.name), "counter");
    assert_eq!(cstr(info.version), "2.5.0");
    assert_eq!(cstr(info.root_path), "/counter");

    // --- lifecycle ---------------------------------------------------------
    assert_eq!(unsafe { host_abi::repe_plugin_init() }, RepeResult::Ok);
    assert_eq!(
        unsafe { host_abi::repe_plugin_init() },
        RepeResult::AlreadyInitialized,
        "a second init reports rather than rebuilding"
    );

    // --- dispatch ----------------------------------------------------------
    let response = host_call(&request(
        "/counter/echo",
        &serde_json::json!({"n": 1}),
        false,
    ))
    .expect("a non-notify request produces a response");
    let message = Message::from_slice(&response).unwrap();
    assert_eq!(message.header.id, 4, "the request id is echoed");
    assert_eq!(message.query_str().unwrap(), "/counter/echo");
    assert_eq!(message.json_body::<serde_json::Value>().unwrap()["n"], 1);

    // Everything the plugin serves really does live under the root it claims,
    // which is what lets a host route by prefix.
    let root = cstr(info.root_path);
    for path in ["/counter/echo", "/counter/value", "/counter/bump"] {
        assert!(path.starts_with(&root));
        let response = host_call(&request(path, &serde_json::json!(1), false)).unwrap();
        assert_ne!(
            Message::from_slice(&response).unwrap().error_code(),
            Some(ErrorCode::MethodNotFound),
            "{path} is served"
        );
    }

    // A field write then a derived method reading and mutating it, which is what
    // makes the object on the far side of the ABI stateful rather than a
    // request-scoped value: both calls reach the same long-lived `Counter`.
    host_call(&request("/counter/value", &serde_json::json!(10), false)).unwrap();
    let response = host_call(&request("/counter/bump", &serde_json::json!(7), false)).unwrap();
    assert_eq!(
        Message::from_slice(&response)
            .unwrap()
            .json_body::<i64>()
            .unwrap(),
        17,
        "the method saw the value the field write left behind"
    );

    // --- notify ------------------------------------------------------------
    assert!(
        host_call(&request("/counter/echo", &serde_json::json!(0), true)).is_none(),
        "a notify answers with size 0, which a host must read as `send nothing`"
    );

    // --- a malformed frame is answered, not ignored -------------------------
    let response = host_call(b"clearly not a repe frame").unwrap();
    let message = Message::from_slice(&response).unwrap();
    assert!(message.is_error());
    assert_eq!(message.header.id, 0, "the id was unreadable");

    // --- concurrent calls, which `plugin.h` explicitly permits ---------------
    // Each thread has its own response buffer, so N threads in flight must not
    // see each other's bytes.
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..8i64)
            .map(|n| {
                scope.spawn(move || {
                    for _ in 0..64 {
                        let response =
                            host_call(&request("/counter/echo", &serde_json::json!(n), false))
                                .unwrap();
                        let echoed = Message::from_slice(&response)
                            .unwrap()
                            .json_body::<i64>()
                            .unwrap();
                        assert_eq!(echoed, n, "thread {n} read another thread's buffer");
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
    });

    // --- shutdown, last, because it is visible to everything above ----------
    unsafe { host_abi::repe_plugin_shutdown() };
    let response = host_call(&request("/counter/echo", &serde_json::json!(0), false)).unwrap();
    let message = Message::from_slice(&response).unwrap();
    assert_eq!(
        message.error_code(),
        Some(ErrorCode::InternalError),
        "a call after shutdown is refused with an error rather than served or ignored"
    );
}

/// The annotated constructor stays callable, so the router that crosses the ABI
/// can be driven in-process with no plugin machinery in the way. That is the
/// difference between a plugin you can unit-test and one you can only integrate.
#[test]
fn the_router_constructor_is_still_a_plain_function() {
    let router = build();
    let response = router
        .call(&request("/counter/echo", &serde_json::json!("hi"), false))
        .expect("a non-notify request produces a response");
    assert_eq!(
        Message::from_slice(&response)
            .unwrap()
            .json_body::<String>()
            .unwrap(),
        "hi"
    );
}
