//! Reads served through a shared borrow.
//!
//! Struct dispatch used to take an exclusive guard for every request, so a
//! `/version` read queued behind whatever long-running call happened to hold the
//! object. A bodiless request — a read, by REPE's own frame-level distinction —
//! now goes to `RepeStruct::repe_read_into` first when the lock has a shared
//! mode, and only falls back to the exclusive path when the struct declines.
//!
//! Two things have to hold, and both are pinned here: a read that *can* be
//! served shared genuinely is (proved by holding a read guard, or by two reads
//! meeting inside one handler), and every read answers byte-for-byte the same
//! whichever path served it (proved by running each path against a `Mutex`,
//! which has no shared mode and so always dispatches the old way).

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Barrier, Mutex, OnceLock, RwLock};
use std::time::Duration;

use repe::RepeStruct;
use repe::constants::{BodyFormat, ErrorCode, QueryFormat};
use repe::message::Message;
use repe::server::Router;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

#[derive(Clone, Default, serde::Serialize, serde::Deserialize, RepeStruct)]
struct Clock {
    ticks: u64,
    source: String,
}

/// One struct carrying every shape the read path has to decide about: plain
/// fields, a typed numeric field, a nested child, a `&self` method, a `&mut self`
/// method, and a field-shaped accessor pair.
#[derive(Clone, Default, serde::Serialize, serde::Deserialize, RepeStruct)]
#[repe(methods)]
struct Instrument {
    firmware: String,
    channel: u32,
    #[repe(typed)]
    gains: Vec<f64>,
    #[repe(nested)]
    clock: Clock,
    calibrations: u32,
    /// A name that has to be escaped in a JSON Pointer (`~` and `/` are the two
    /// characters RFC 6901 escapes), so the read path's segment parsing is
    /// covered on its slow branch as well as its fast one.
    #[repe(rename = "odd/name~x")]
    oddly_named: u32,
}

#[repe::methods]
impl Instrument {
    /// Servable under a shared borrow: it only reads.
    fn identify(&self) -> String {
        format!("instrument fw {}", self.firmware)
    }

    /// Not servable: it mutates.
    fn calibrate(&mut self, gain: f64) -> f64 {
        self.calibrations += 1;
        gain / 2.0
    }

    #[repe(get = "channel_hz")]
    fn channel_hz(&self) -> f64 {
        f64::from(self.channel) * 1.0e6
    }

    #[repe(set = "channel_hz")]
    fn set_channel_hz(&mut self, hz: f64) {
        self.channel = (hz / 1.0e6) as u32;
    }

    /// A field-shaped endpoint that is also a typed numeric array.
    #[repe(get = "trims")]
    #[repe(typed)]
    fn trims(&self) -> Vec<f64> {
        self.gains.iter().map(|gain| gain * 0.5).collect()
    }

    /// A `&self` method with nothing to return, which is the one read the
    /// encoding path answers with a bare `null`.
    fn ping(&self) {}

    /// The same, fallible.
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

fn instrument() -> Instrument {
    Instrument {
        firmware: String::from("4.2.0"),
        channel: 6,
        gains: vec![1.0, 2.5, 4.0],
        clock: Clock {
            ticks: 990,
            source: String::from("gps"),
        },
        calibrations: 0,
        oddly_named: 17,
    }
}

/// A struct whose only accessor has a `&mut self` getter, so every read of it
/// declines. Nested below, it is what forces a parent's listing to rewind an
/// object it had already started writing.
#[derive(Clone, Default, serde::Serialize, serde::Deserialize, RepeStruct)]
#[repe(methods)]
struct Counter {
    label: String,
    hits: u32,
}

#[repe::methods]
impl Counter {
    /// Counts its own reads, which is what makes it `&mut self`.
    #[repe(get = "reads")]
    fn reads(&mut self) -> u32 {
        self.hits += 1;
        self.hits
    }
}

#[derive(Clone, Default, serde::Serialize, serde::Deserialize, RepeStruct)]
struct Rack {
    name: String,
    #[repe(nested)]
    counter: Counter,
}

fn rack() -> Rack {
    Rack {
        name: String::from("rack-1"),
        counter: Counter {
            label: String::from("primary"),
            hits: 0,
        },
    }
}

/// A `&self` getter and a `&self` method that both fail. The shared path serves
/// them, so it is the path that has to turn an `Err` into an error frame — and
/// has to rewind the listing it was partway through when it does.
#[derive(Clone, Default, serde::Serialize, serde::Deserialize, RepeStruct)]
#[repe(methods)]
struct Faulty {
    label: String,
}

#[repe::methods]
impl Faulty {
    #[repe(get = "reading")]
    fn reading(&self) -> Result<f64, String> {
        Err(String::from("sensor offline"))
    }

    fn probe(&self) -> Result<u32, String> {
        Err(String::from("bus timeout"))
    }
}

// ---------------------------------------------------------------------------
// Frame helpers
// ---------------------------------------------------------------------------

fn read(query: &str) -> Vec<u8> {
    Message::builder()
        .id(1)
        .query_str(query)
        .query_format(QueryFormat::JsonPointer)
        .build()
        .to_vec()
}

fn write<T: serde::Serialize>(query: &str, value: &T) -> Vec<u8> {
    Message::builder()
        .id(1)
        .query_str(query)
        .query_format(QueryFormat::JsonPointer)
        .body_json(value)
        .expect("the fixtures serialize")
        .build()
        .to_vec()
}

fn answer(router: &Router, request: &[u8]) -> Message {
    let frame = router
        .call(request)
        .expect("a non-notify request is answered");
    Message::from_slice(&frame).expect("the response is a REPE frame")
}

/// Every read shape the fixtures publish, exercised against both lock kinds.
const READ_PATHS: &[&str] = &[
    "/inst",
    "/inst/firmware",
    "/inst/channel",
    "/inst/gains",
    "/inst/clock",
    "/inst/clock/ticks",
    "/inst/identify",
    "/inst/channel_hz",
    "/inst/trims",
    "/inst/ping",
    "/inst/check",
    "/inst/odd~1name~0x",
    // Reads that are errors, which the shared path has to report identically
    // rather than approximate.
    "/inst/calibrate",
    "/inst/missing",
    "/inst/firmware/extra",
    "/inst/gains/0",
    "/inst/",
    // Deeper than the stack buffer `with_segments` fills before it spills to a
    // `Vec`, so the overflow branch is on the oracle too.
    "/inst/a/b/c/d/e/f/g/h/i/j/k/l/m/n/o/p/q/r",
];

// ---------------------------------------------------------------------------
// The two paths agree
// ---------------------------------------------------------------------------

#[test]
fn every_read_answers_the_same_under_a_mutex_and_an_rwlock() {
    let exclusive = Router::new()
        .with_struct_shared::<Instrument, _>("/inst", Arc::new(Mutex::new(instrument())));
    let shared = Router::new()
        .with_struct_shared::<Instrument, _>("/inst", Arc::new(RwLock::new(instrument())));

    for path in READ_PATHS {
        let request = read(path);
        assert_eq!(
            exclusive.call(&request),
            shared.call(&request),
            "reading `{path}` through a shared guard must produce the frame the exclusive guard \
             produces, byte for byte"
        );
    }
}

#[test]
fn a_listing_that_has_to_decline_answers_the_same_as_the_exclusive_path() {
    // `Rack`'s nested `Counter` declines every read, so the shared attempt gets
    // partway through the object and then rewinds. What the client sees must be
    // indistinguishable from the exclusive path's answer.
    let exclusive =
        Router::new().with_struct_shared::<Rack, _>("/rack", Arc::new(Mutex::new(rack())));
    let shared =
        Router::new().with_struct_shared::<Rack, _>("/rack", Arc::new(RwLock::new(rack())));

    for path in [
        "/rack",
        "/rack/counter",
        "/rack/counter/reads",
        "/rack/name",
    ] {
        let request = read(path);
        assert_eq!(
            exclusive.call(&request),
            shared.call(&request),
            "reading `{path}` must not depend on which guard served it"
        );
    }
}

#[test]
fn a_failing_shared_getter_reports_the_same_error_either_way() {
    let faulty = || Faulty {
        label: String::from("probe-1"),
    };
    let exclusive =
        Router::new().with_struct_shared::<Faulty, _>("/f", Arc::new(Mutex::new(faulty())));
    let shared =
        Router::new().with_struct_shared::<Faulty, _>("/f", Arc::new(RwLock::new(faulty())));

    // The listing included: a getter that fails fails the whole read, which is
    // the field analogy held to consistently, and the shared path must reach
    // the same frame rather than a half-written object.
    for path in ["/f", "/f/reading", "/f/probe", "/f/label"] {
        let request = read(path);
        assert_eq!(
            exclusive.call(&request),
            shared.call(&request),
            "reading `{path}` must not depend on which guard served it"
        );
    }
    assert_eq!(
        answer(&shared, &read("/f/reading")).error_code(),
        Some(ErrorCode::ParseError)
    );
}

#[test]
fn a_declined_listing_leaves_no_half_written_body_behind() {
    let shared =
        Router::new().with_struct_shared::<Rack, _>("/rack", Arc::new(RwLock::new(rack())));
    let message = answer(&shared, &read("/rack"));

    // The rewind is only correct if the body parses as one whole object with
    // exactly the expected keys: a leftover `{"name":"rack-1",` would not.
    let value = message
        .json_body::<serde_json::Value>()
        .expect("the listing body is valid JSON");
    assert_eq!(
        value,
        serde_json::json!({
            "name": "rack-1",
            "counter": { "label": "primary", "hits": 0, "reads": 1 },
        })
    );
}

// ---------------------------------------------------------------------------
// The shared path is genuinely taken
// ---------------------------------------------------------------------------

/// Run `f` on another thread and wait `timeout` for it to finish.
fn try_off_thread<T, F>(timeout: Duration, f: F) -> Result<T, mpsc::RecvTimeoutError>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(timeout)
}

#[test]
fn every_shareable_read_proceeds_while_a_read_guard_is_held() {
    // Holding a read guard for the whole test is the direct proof: anything the
    // exclusive path had to serve would block here rather than answer. This is
    // the set the equivalence test cannot distinguish on its own.
    //
    // `/inst` itself is absent deliberately: `Instrument` publishes field-shaped
    // endpoints, so its whole-object listing declines. See
    // `a_listing_with_a_field_shaped_endpoint_declines`.
    let state = Arc::new(RwLock::new(instrument()));
    let router = Router::new().with_struct_shared::<Instrument, _>("/inst", Arc::clone(&state));
    let held = state.read().expect("the lock is not poisoned");

    for path in [
        "/inst/firmware",
        "/inst/gains",
        "/inst/odd~1name~0x",
        // A nested listing, which has nothing to invoke and so is served shared.
        "/inst/clock",
        "/inst/clock/ticks",
        "/inst/identify",
        "/inst/ping",
        "/inst/channel_hz",
        "/inst/trims",
    ] {
        let router = router.clone();
        let frame = try_off_thread(Duration::from_secs(10), move || router.call(&read(path)))
            .unwrap_or_else(|_| panic!("reading `{path}` must not wait for the exclusive guard"))
            .expect("a non-notify request is answered");
        let message = Message::from_slice(&frame).expect("the response is a REPE frame");
        assert!(
            !message.is_error(),
            "reading `{path}` answered with an error"
        );
    }

    drop(held);
}

#[test]
fn a_write_still_waits_for_the_exclusive_guard() {
    let state = Arc::new(RwLock::new(instrument()));
    let router = Router::new().with_struct_shared::<Instrument, _>("/inst", Arc::clone(&state));

    let held = state.read().expect("the lock is not poisoned");
    let (tx, rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let _ = tx.send(router.call(&write("/inst/channel", &9u32)));
    });
    assert!(
        rx.recv_timeout(Duration::from_millis(200)).is_err(),
        "a write carries a body, so it is not a read and must not slip past the exclusive guard"
    );
    drop(held);

    rx.recv_timeout(Duration::from_secs(10))
        .expect("the write completes once the guard is released");
    worker.join().expect("the worker thread finishes");
    assert_eq!(state.read().unwrap().channel, 9);
}

#[test]
fn a_read_that_declines_still_waits_for_the_exclusive_guard() {
    // `reads` has a `&mut self` getter: the shared attempt declines it, and the
    // exclusive retry is what actually serves it.
    let state = Arc::new(RwLock::new(Counter {
        label: String::from("primary"),
        hits: 0,
    }));
    let router = Router::new().with_struct_shared::<Counter, _>("/counter", Arc::clone(&state));

    let held = state.read().expect("the lock is not poisoned");
    let (tx, rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let _ = tx.send(router.call(&read("/counter/reads")));
    });
    assert!(
        rx.recv_timeout(Duration::from_millis(200)).is_err(),
        "a `&mut self` getter cannot be served through a shared borrow"
    );
    drop(held);

    let frame = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("the read completes once the guard is released")
        .expect("a non-notify request is answered");
    let message = Message::from_slice(&frame).expect("the response is a REPE frame");
    assert_eq!(message.json_body::<u32>().unwrap(), 1);
    worker.join().expect("the worker thread finishes");
}

/// A rendezvous the two readers below meet at. Reaching it at all proves both
/// were inside the handler at once; under an exclusive guard the second would
/// still be waiting for the lock.
fn rendezvous() -> &'static Barrier {
    static BARRIER: OnceLock<Barrier> = OnceLock::new();
    BARRIER.get_or_init(|| Barrier::new(2))
}

#[derive(Clone, Default, serde::Serialize, serde::Deserialize, RepeStruct)]
#[repe(methods)]
struct Gate {
    opened: u32,
}

#[repe::methods]
impl Gate {
    fn meet(&self) -> u32 {
        rendezvous().wait();
        self.opened
    }
}

#[test]
fn two_reads_of_one_struct_run_at_the_same_time() {
    let router = Router::new()
        .with_struct_shared::<Gate, _>("/gate", Arc::new(RwLock::new(Gate { opened: 7 })));

    let (tx, rx) = mpsc::channel();
    for _ in 0..2 {
        let router = router.clone();
        let tx = tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(router.call(&read("/gate/meet")));
        });
    }
    drop(tx);

    for _ in 0..2 {
        let frame = rx
            .recv_timeout(Duration::from_secs(10))
            .expect(
                "both readers must be inside the handler at once, which only a shared guard allows",
            )
            .expect("a non-notify request is answered");
        let message = Message::from_slice(&frame).expect("the response is a REPE frame");
        assert_eq!(message.json_body::<u32>().unwrap(), 7);
    }
}

// ---------------------------------------------------------------------------
// Shapes worth naming on their own
// ---------------------------------------------------------------------------

#[test]
fn a_typed_field_read_shared_is_still_beve() {
    let router = Router::new()
        .with_struct_shared::<Instrument, _>("/inst", Arc::new(RwLock::new(instrument())));
    let message = answer(&router, &read("/inst/gains"));
    assert_eq!(
        BodyFormat::try_from(message.header.body_format),
        Ok(BodyFormat::Beve),
        "`#[repe(typed)]` still routes to the bulk encoding when the read is served shared"
    );
    assert_eq!(
        beve::from_slice::<Vec<f64>>(&message.body).unwrap(),
        vec![1.0, 2.5, 4.0]
    );
}

#[test]
fn a_shared_method_and_accessor_are_served_shared() {
    let router = Router::new()
        .with_struct_shared::<Instrument, _>("/inst", Arc::new(RwLock::new(instrument())));
    assert_eq!(
        answer(&router, &read("/inst/identify"))
            .json_body::<String>()
            .unwrap(),
        "instrument fw 4.2.0"
    );
    assert_eq!(
        answer(&router, &read("/inst/channel_hz"))
            .json_body::<f64>()
            .unwrap(),
        6.0e6
    );
}

#[test]
fn writes_and_mutating_methods_still_work_under_an_rwlock() {
    let state = Arc::new(RwLock::new(instrument()));
    let router = Router::new().with_struct_shared::<Instrument, _>("/inst", Arc::clone(&state));

    answer(&router, &write("/inst/channel", &3u32));
    assert_eq!(state.read().unwrap().channel, 3);

    // The setter half of a field-shaped endpoint.
    answer(&router, &write("/inst/channel_hz", &8.0e6f64));
    assert_eq!(state.read().unwrap().channel, 8);

    let halved = answer(&router, &write("/inst/calibrate", &9.0f64))
        .json_body::<f64>()
        .unwrap();
    assert_eq!(halved, 4.5);
    assert_eq!(state.read().unwrap().calibrations, 1);
}

#[test]
fn a_bodiless_call_to_a_method_that_needs_arguments_is_still_body_expected() {
    let router = Router::new()
        .with_struct_shared::<Instrument, _>("/inst", Arc::new(RwLock::new(instrument())));
    let message = answer(&router, &read("/inst/calibrate"));
    assert_eq!(message.error_code(), Some(ErrorCode::InvalidBody));
}

#[test]
fn an_unknown_path_is_method_not_found_on_the_shared_path() {
    let router = Router::new()
        .with_struct_shared::<Instrument, _>("/inst", Arc::new(RwLock::new(instrument())));
    assert_eq!(
        answer(&router, &read("/inst/missing")).error_code(),
        Some(ErrorCode::MethodNotFound)
    );
    assert_eq!(
        answer(&router, &read("/inst/firmware/extra")).error_code(),
        Some(ErrorCode::MethodNotFound)
    );
}

// ---------------------------------------------------------------------------
// A listing never invokes a published getter
// ---------------------------------------------------------------------------

/// A `&self` getter with an observable side effect, beside a `&mut self` getter
/// that cannot be served shared.
///
/// This is the shape that made speculative listings unsafe: the shared attempt
/// walked the entries in order, called `sample`, reached `calibration`, gave up,
/// and the exclusive retry called `sample` again — so the very first read of the
/// object reported `2`. A listing now declines before it calls anything.
#[derive(Default, serde::Serialize, serde::Deserialize, RepeStruct)]
#[repe(methods)]
struct Meter {
    name: String,
    #[repe(skip)]
    #[serde(skip)]
    samples: AtomicU32,
}

#[repe::methods]
impl Meter {
    #[repe(get = "sample")]
    fn sample(&self) -> u32 {
        self.samples.fetch_add(1, Ordering::SeqCst) + 1
    }

    #[repe(get = "calibration")]
    fn calibration(&mut self) -> u32 {
        0
    }
}

#[test]
fn a_listing_invokes_each_getter_exactly_once() {
    for (kind, router) in [
        (
            "Mutex",
            Router::new()
                .with_struct_shared::<Meter, _>("/m", Arc::new(Mutex::new(Meter::default()))),
        ),
        (
            "RwLock",
            Router::new()
                .with_struct_shared::<Meter, _>("/m", Arc::new(RwLock::new(Meter::default()))),
        ),
    ] {
        let value = answer(&router, &read("/m"))
            .json_body::<serde_json::Value>()
            .expect("the listing body is valid JSON");
        assert_eq!(
            value["sample"], 1,
            "under a {kind} the first listing read must report the first sample; a getter run \
             twice for one request reports the second"
        );
    }
}

#[test]
fn a_listing_with_a_field_shaped_endpoint_declines() {
    // The trade the rule above buys: a struct with an accessor gives up the
    // shared listing, though every one of its individual reads keeps it.
    let state = Arc::new(RwLock::new(instrument()));
    let router = Router::new().with_struct_shared::<Instrument, _>("/inst", Arc::clone(&state));

    let held = state.read().expect("the lock is not poisoned");
    let (tx, rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let _ = tx.send(router.call(&read("/inst")));
    });
    assert!(
        rx.recv_timeout(Duration::from_millis(200)).is_err(),
        "a listing that would have to invoke a getter takes the exclusive guard"
    );
    drop(held);

    rx.recv_timeout(Duration::from_secs(10))
        .expect("the listing completes once the guard is released");
    worker.join().expect("the worker thread finishes");
}

#[test]
fn a_listing_with_nothing_to_invoke_is_served_shared() {
    let state = Arc::new(RwLock::new(Clock {
        ticks: 990,
        source: String::from("gps"),
    }));
    let router = Router::new().with_struct_shared::<Clock, _>("/clock", Arc::clone(&state));

    let held = state.read().expect("the lock is not poisoned");
    let frame = try_off_thread(Duration::from_secs(10), move || {
        router.call(&read("/clock"))
    })
    .expect("a listing of fields alone must not wait for the exclusive guard")
    .expect("a non-notify request is answered");
    drop(held);

    let message = Message::from_slice(&frame).expect("the response is a REPE frame");
    assert_eq!(
        message.json_body::<serde_json::Value>().unwrap(),
        serde_json::json!({ "ticks": 990, "source": "gps" })
    );
}

// ---------------------------------------------------------------------------
// The remaining published read shapes, against the same oracle
// ---------------------------------------------------------------------------

/// The struct-attribute method list, the other way a method reaches the wire.
/// Its receivers are declared by hand rather than read off a signature, and the
/// read path is what acts on that declaration.
#[derive(Clone, Default, serde::Serialize, serde::Deserialize, RepeStruct)]
#[repe(methods(
    describe(&self) -> String,
    bump(&mut self) -> u32,
    scale(&self, factor: f64) -> f64
))]
struct Listed {
    gain: f64,
}

impl Listed {
    fn describe(&self) -> String {
        format!("gain {}", self.gain)
    }
    fn bump(&mut self) -> u32 {
        self.gain += 1.0;
        self.gain as u32
    }
    fn scale(&self, factor: f64) -> f64 {
        self.gain * factor
    }
}

#[test]
fn the_attribute_list_form_answers_the_same_either_way() {
    let listed = || Listed { gain: 2.0 };
    let exclusive =
        Router::new().with_struct_shared::<Listed, _>("/l", Arc::new(Mutex::new(listed())));
    let shared =
        Router::new().with_struct_shared::<Listed, _>("/l", Arc::new(RwLock::new(listed())));

    for path in ["/l", "/l/gain", "/l/describe", "/l/bump", "/l/scale"] {
        let request = read(path);
        assert_eq!(
            exclusive.call(&request),
            shared.call(&request),
            "reading `{path}` must not depend on which guard served it"
        );
    }
}

#[derive(Clone, Default, serde::Serialize, serde::Deserialize, RepeStruct)]
struct Bay {
    slot: u32,
    #[repe(nested)]
    sensor: Faulty,
}

#[test]
fn an_error_from_a_nested_child_is_prefixed_the_same_either_way() {
    let bay = || Bay {
        slot: 4,
        sensor: Faulty {
            label: String::from("probe-1"),
        },
    };
    let exclusive = Router::new().with_struct_shared::<Bay, _>("/bay", Arc::new(Mutex::new(bay())));
    let shared = Router::new().with_struct_shared::<Bay, _>("/bay", Arc::new(RwLock::new(bay())));

    for path in [
        "/bay",
        "/bay/sensor",
        "/bay/sensor/reading",
        "/bay/sensor/probe",
    ] {
        let request = read(path);
        assert_eq!(
            exclusive.call(&request),
            shared.call(&request),
            "reading `{path}` must not depend on which guard served it"
        );
    }
    let text = answer(&shared, &read("/bay/sensor/reading"))
        .error_message_utf8()
        .expect("a failing getter answers with an error frame");
    assert!(
        text.contains("/sensor/reading"),
        "the error path is prefixed with the nested field, got `{text}`"
    );
}

#[cfg(feature = "parking-lot")]
#[test]
fn parking_lot_rwlock_answers_the_same_as_a_mutex() {
    let exclusive = Router::new()
        .with_struct_shared::<Instrument, _>("/inst", Arc::new(Mutex::new(instrument())));
    let shared = Router::new().with_struct_shared::<Instrument, _>(
        "/inst",
        Arc::new(parking_lot::RwLock::new(instrument())),
    );

    for path in READ_PATHS {
        let request = read(path);
        assert_eq!(
            exclusive.call(&request),
            shared.call(&request),
            "reading `{path}` through a `parking_lot::RwLock` must match the exclusive frame"
        );
    }
}
