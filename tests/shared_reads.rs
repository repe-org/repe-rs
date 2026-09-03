//! Requests served through a shared borrow.
//!
//! Struct dispatch used to take an exclusive guard for every request, so a
//! `/version` read queued behind whatever long-running call happened to hold the
//! object. Every request now goes to `RepeStruct::repe_shared_into` first when
//! the lock has a shared mode, and only falls back to the exclusive path when
//! the struct declines.
//!
//! **What decides is the receiver, not the frame.** The first version of this
//! asked the frame — a request with no body is a read — which is REPE's own
//! distinction and the wrong one here: a `&self` method taking arguments carries
//! a body, so it took the write guard and stalled every read of the object for
//! as long as it ran. One long-running `&self` call turned a sub-millisecond
//! `/version` read into one that waited for the whole call. The receiver is
//! known where the dispatch arms are generated, so it is the receiver that
//! answers.
//!
//! Two things have to hold, and both are pinned here: a request that *can* be
//! served shared genuinely is (proved by holding a read guard, or by two calls
//! meeting inside one handler), and every request answers byte-for-byte the same
//! whichever path served it (proved by running each against a `Mutex`, which has
//! no shared mode and so always dispatches the old way).

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

#[derive(Clone, Default, RepeStruct)]
struct Clock {
    ticks: u64,
    source: String,
}
structio::object!(Clock { ticks, source });

/// One struct carrying every shape the read path has to decide about: plain
/// fields, a typed numeric field, a nested child, a `&self` method, a `&mut self`
/// method, and a field-shaped accessor pair.
#[derive(Clone, Default, RepeStruct)]
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
structio::object!(Instrument { firmware, channel, gains, clock, calibrations, "odd/name~x" => oddly_named });

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

    /// A `&self` method that takes an argument — the shape the frame-level rule
    /// got wrong. It carries a body, so REPE's frame distinction calls it a
    /// write; it takes `&self`, so it is a call and mutates nothing.
    fn scale(&self, factor: f64) -> Vec<f64> {
        self.gains.iter().map(|gain| gain * factor).collect()
    }

    /// The same with two arguments, so `MethodArgs` decoding — positional and
    /// name-keyed both — is on the shared path as well as the exclusive one.
    fn window(&self, lo: usize, hi: usize) -> Vec<f64> {
        self.gains
            .get(lo..hi)
            .map(<[f64]>::to_vec)
            .unwrap_or_default()
    }

    /// `&self`, takes an argument, and can fail: the shared path has to turn an
    /// `Err` from an argument-taking call into the same error frame.
    fn verify(&self, expected: String) -> Result<(), String> {
        if expected == self.firmware {
            Ok(())
        } else {
            Err(format!("firmware is {firmware}", firmware = self.firmware))
        }
    }

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
#[derive(Clone, Default, RepeStruct)]
#[repe(methods)]
struct Counter {
    label: String,
    hits: u32,
}
structio::object!(Counter { label, hits });

#[repe::methods]
impl Counter {
    /// Counts its own reads, which is what makes it `&mut self`.
    #[repe(get = "reads")]
    fn reads(&mut self) -> u32 {
        self.hits += 1;
        self.hits
    }
}

#[derive(Clone, Default, RepeStruct)]
struct Rack {
    name: String,
    #[repe(nested)]
    counter: Counter,
}
structio::object!(Rack { name, counter });

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
#[derive(Clone, Default, RepeStruct)]
#[repe(methods)]
struct Faulty {
    label: String,
}
structio::object!(Faulty { label });

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

/// A write whose body is the given JSON text, sent verbatim.
///
/// These tests are about which lock served a request and what a body did, so
/// the body is the text a client would send rather than a declared type.
fn write(query: &str, body: &str) -> Vec<u8> {
    Message::builder()
        .id(1)
        .query_str(query)
        .query_format(QueryFormat::JsonPointer)
        .body_bytes(body.as_bytes().to_vec())
        .body_format(BodyFormat::Json)
        .build()
        .to_vec()
}

/// The response body as JSON text.
fn body_text(message: &Message) -> &str {
    std::str::from_utf8(&message.body).expect("the response body is UTF-8 JSON")
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

/// Every body-carrying shape the fixtures publish, in a fixed sequence so both
/// routers walk the same state transitions and can be compared frame for frame.
///
/// Half of these are now served by the shared borrow — every `&self` call — and
/// half still take the exclusive guard. Which is which must not be visible in
/// the answer.
fn write_frames() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        // Served shared: `&self`, arguments and all.
        ("scale", write("/inst/scale", "2.0")),
        ("window positional", write("/inst/window", "[0,2]")),
        (
            "window named",
            write("/inst/window", r##"{ "lo": 1, "hi": 3 }"##),
        ),
        ("verify ok", write("/inst/verify", r##""4.2.0""##)),
        ("verify err", write("/inst/verify", r##""0.0.0""##)),
        // A method taking no arguments ignores a body, on either path.
        ("identify with a body", write("/inst/identify", "1")),
        // Still exclusive: writes and `&mut self` calls.
        ("field write", write("/inst/channel", "9")),
        ("accessor write", write("/inst/channel_hz", "8.0e6")),
        ("mutating call", write("/inst/calibrate", "9.0")),
        (
            "whole child write",
            write("/inst/clock", r##"{ "ticks": 5, "source": "pps" }"##),
        ),
        ("nested field write", write("/inst/clock/ticks", "7")),
        // Errors, which must also not depend on the guard that produced them.
        (
            "scale with a bad body",
            write("/inst/scale", r##""not a number""##),
        ),
        ("window too short", write("/inst/window", "[1]")),
        ("subpath below a leaf", write("/inst/gains/0", "1.0")),
        ("unknown path", write("/inst/missing", "1")),
    ]
}

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
fn every_write_answers_the_same_under_a_mutex_and_an_rwlock() {
    let exclusive = Router::new()
        .with_struct_shared::<Instrument, _>("/inst", Arc::new(Mutex::new(instrument())));
    let shared = Router::new()
        .with_struct_shared::<Instrument, _>("/inst", Arc::new(RwLock::new(instrument())));

    for (name, request) in write_frames() {
        assert_eq!(
            exclusive.call(&request),
            shared.call(&request),
            "`{name}` through a shared guard must produce the frame the exclusive guard \
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
    assert_eq!(
        body_text(&message),
        r#"{"name":"rack-1","counter":{"label":"primary","hits":0,"reads":1}}"#
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
    // `/inst` itself is here: `Instrument` publishes field-shaped endpoints, but
    // both getters take `&self`, so nothing in its listing can decline and the
    // listing is served shared. What forces the exclusive path is a `&mut self`
    // getter, not an accessor — see `a_listing_with_a_mutating_getter_declines`.
    let state = Arc::new(RwLock::new(instrument()));
    let router = Router::new().with_struct_shared::<Instrument, _>("/inst", Arc::clone(&state));
    let held = state.read().expect("the lock is not poisoned");

    for path in [
        "/inst",
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
fn a_self_call_carrying_arguments_proceeds_while_a_read_guard_is_held() {
    // The frame carries a body, so REPE's frame-level distinction calls each of
    // these a write. The receiver says otherwise, and the receiver is what the
    // generated arm reads. Anything still deciding from the frame would block
    // here rather than answer.
    let state = Arc::new(RwLock::new(instrument()));
    let router = Router::new().with_struct_shared::<Instrument, _>("/inst", Arc::clone(&state));
    let held = state.read().expect("the lock is not poisoned");

    for (name, request) in [
        ("scale", write("/inst/scale", "2.0")),
        ("window", write("/inst/window", "[0,2]")),
        ("verify", write("/inst/verify", r##""4.2.0""##)),
    ] {
        let router = router.clone();
        let frame = try_off_thread(Duration::from_secs(10), move || router.call(&request))
            .unwrap_or_else(|_| {
                panic!(
                    "`{name}` takes `&self`, so carrying a body must not put it behind the \
                        exclusive guard"
                )
            })
            .expect("a non-notify request is answered");
        let message = Message::from_slice(&frame).expect("the response is a REPE frame");
        assert!(!message.is_error(), "`{name}` answered with an error");
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
        let _ = tx.send(router.call(&write("/inst/channel", "9")));
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
fn a_mutating_call_carrying_arguments_still_waits_for_the_exclusive_guard() {
    // The other half of the rule. `calibrate` takes `&mut self`, so the shared
    // attempt declines it — without taking the body, which is what lets the
    // exclusive retry dispatch the same request rather than a bodiless one.
    let state = Arc::new(RwLock::new(instrument()));
    let router = Router::new().with_struct_shared::<Instrument, _>("/inst", Arc::clone(&state));

    let held = state.read().expect("the lock is not poisoned");
    let (tx, rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let _ = tx.send(router.call(&write("/inst/calibrate", "9.0")));
    });
    assert!(
        rx.recv_timeout(Duration::from_millis(200)).is_err(),
        "a `&mut self` method cannot run under a shared borrow, arguments or not"
    );
    drop(held);

    let frame = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("the call completes once the guard is released")
        .expect("a non-notify request is answered");
    let message = Message::from_slice(&frame).expect("the response is a REPE frame");
    assert_eq!(
        message.json_body::<f64>().unwrap(),
        4.5,
        "the exclusive retry must see the body the shared attempt declined to take"
    );
    worker.join().expect("the worker thread finishes");
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

#[derive(Clone, Default, RepeStruct)]
#[repe(methods)]
struct Gate {
    opened: u32,
}
structio::object!(Gate { opened });

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

/// The two barriers the slow-call test below meets at: one to prove the call is
/// genuinely inside the handler before the read is attempted, one to let it
/// finish afterwards.
fn slow_call_gates() -> &'static (Barrier, Barrier) {
    static GATES: OnceLock<(Barrier, Barrier)> = OnceLock::new();
    GATES.get_or_init(|| (Barrier::new(2), Barrier::new(2)))
}

/// The friction this rule exists to remove, in miniature: a `&self` call that
/// takes arguments and runs for a long time, next to a plain read of the same
/// object.
#[derive(Clone, Default, RepeStruct)]
#[repe(methods)]
struct Slow {
    version: String,
}
structio::object!(Slow { version });

#[repe::methods]
impl Slow {
    /// Stands in for a call that runs for a long time: `&self`, takes a list of
    /// arguments, and does not return until the test says so.
    fn summarize(&self, items: Vec<u32>) -> usize {
        slow_call_gates().0.wait();
        slow_call_gates().1.wait();
        items.len()
    }
}

#[test]
fn a_version_read_is_answered_while_a_slow_self_call_runs() {
    let router = Router::new().with_struct_shared::<Slow, _>(
        "/slow",
        Arc::new(RwLock::new(Slow {
            version: String::from("4.2.0"),
        })),
    );

    let running = {
        let router = router.clone();
        std::thread::spawn(move || router.call(&write("/slow/summarize", "[1,2,3]")))
    };
    // The call is inside the handler from here on, so a read that answers below
    // answered *during* it rather than before it started.
    slow_call_gates().0.wait();

    let frame = try_off_thread(Duration::from_secs(10), move || {
        router.call(&read("/slow/version"))
    })
    .expect("a `/version` read must not queue behind a long `&self` call")
    .expect("a non-notify request is answered");
    let message = Message::from_slice(&frame).expect("the response is a REPE frame");
    assert_eq!(message.json_body::<String>().unwrap(), "4.2.0");

    slow_call_gates().1.wait();
    let frame = running
        .join()
        .expect("the worker thread finishes")
        .expect("a non-notify request is answered");
    let message = Message::from_slice(&frame).expect("the response is a REPE frame");
    assert_eq!(message.json_body::<usize>().unwrap(), 3);
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
        structio::from_beve::<Vec<f64>>(&message.body).unwrap(),
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

    answer(&router, &write("/inst/channel", "3"));
    assert_eq!(state.read().unwrap().channel, 3);

    // The setter half of a field-shaped endpoint.
    answer(&router, &write("/inst/channel_hz", "8.0e6"));
    assert_eq!(state.read().unwrap().channel, 8);

    let halved = answer(&router, &write("/inst/calibrate", "9.0"))
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
#[derive(Default, RepeStruct)]
#[repe(methods)]
struct Meter {
    name: String,
    #[repe(skip)]
    samples: AtomicU32,
}
structio::object!(Meter { name, .. });

/// A struct whose only accessor is a `&self` getter **with a side effect**, so
/// its own listing is served shared *and invokes something*. That combination is
/// what makes a nested decline elsewhere in the tree observable, and it is the
/// shape `Meter` cannot provide — `Meter` has a `&mut self` getter of its own, so
/// it declines before `sample` is ever reached.
#[derive(Default, RepeStruct)]
#[repe(methods)]
struct Tally {
    label: String,
    #[repe(skip)]
    reads: AtomicU32,
}
structio::object!(Tally { label, .. });

#[repe::methods]
impl Tally {
    /// `&self`, and counts anyway — interior mutability is what makes a read
    /// counter possible and a double invocation visible.
    #[repe(get = "count")]
    fn count(&self) -> u32 {
        self.reads.fetch_add(1, Ordering::SeqCst) + 1
    }
}

impl Clone for Tally {
    /// `AtomicU32` is not `Clone`, and the count is per-instance anyway: a clone
    /// starts its own tally rather than inheriting one.
    fn clone(&self) -> Self {
        Self {
            label: self.label.clone(),
            reads: AtomicU32::new(self.reads.load(Ordering::SeqCst)),
        }
    }
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
        let message = answer(&router, &read("/m"));
        let value = body_text(&message);
        assert!(
            value.contains(r#""sample":1"#),
            "under a {kind} the first listing read must report the first sample; a getter run \
             twice for one request reports the second"
        );
    }
}

#[test]
fn a_listing_with_a_mutating_getter_declines() {
    // What the rule above actually costs, and it is narrower than "a struct with
    // an accessor": `Meter::calibration` takes `&mut self`, so a decline is
    // reachable partway through the listing and the whole listing declines at
    // the top instead. A struct whose getters all read keeps its shared listing
    // — that is `a_listing_whose_getters_only_read_is_served_shared`.
    let state = Arc::new(RwLock::new(Meter::default()));
    let router = Router::new().with_struct_shared::<Meter, _>("/m", Arc::clone(&state));

    let held = state.read().expect("the lock is not poisoned");
    let (tx, rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let _ = tx.send(router.call(&read("/m")));
    });
    assert!(
        rx.recv_timeout(Duration::from_millis(200)).is_err(),
        "a listing that could decline partway through takes the exclusive guard"
    );
    drop(held);

    rx.recv_timeout(Duration::from_secs(10))
        .expect("the listing completes once the guard is released");
    worker.join().expect("the worker thread finishes");
}

#[test]
fn a_listing_whose_getters_only_read_carries_their_values() {
    // That this listing is served *shared* is pinned by the `/inst` entry in
    // `every_shareable_read_proceeds_while_a_read_guard_is_held`, which is the
    // systematic place for it. What is left to check is the content: a shared
    // listing shows each accessor's value, not a signature, and not nothing.
    let router = Router::new()
        .with_struct_shared::<Instrument, _>("/inst", Arc::new(RwLock::new(instrument())));

    let message = answer(&router, &read("/inst"));
    let value = body_text(&message);
    assert!(value.contains(r#""channel_hz":6000000"#), "{value}");
    assert!(value.contains(r#""trims":[0.5,1.25,2]"#), "{value}");
    assert!(
        value.contains(r#""identify":"fn(&self) -> String""#),
        "a published method is still listed by its signature, not its result: {value}"
    );
}

/// An ancestor of a struct that publishes field-shaped endpoints. The decline is
/// transitive, so this is what a getter's receiver costs — or does not cost —
/// two levels up.
///
/// The field order is load-bearing: `meter` is listed *before* `counter`, so a
/// listing that decided per struct rather than per subtree would invoke
/// `Meter::sample` and only then discover that `Counter` declines. See
/// `a_declining_sibling_does_not_make_an_earlier_getter_run_twice`.
#[derive(Clone, Default, RepeStruct)]
struct Bench {
    site: String,
    #[repe(nested)]
    inst: Instrument,
    #[repe(nested)]
    tally: Tally,
    #[repe(nested)]
    counter: Counter,
}
structio::object!(Bench {
    site,
    inst,
    tally,
    counter
});

/// A hand-written `RepeStruct`: it overrides neither `repe_shared_into` nor
/// `repe_listing_declines`, so it declines every shared read and says so. This
/// is the ordinary shape — most hand-written impls are exactly this — and it is
/// the second way a nested child can force a parent's listing exclusive, with no
/// accessor anywhere in sight.
#[derive(Default)]
struct Manual {
    note: String,
}
structio::object!(Manual { note });

impl RepeStruct for Manual {
    fn repe_handle_into(
        &mut self,
        segments: &[&str],
        _body: Option<repe::structs::RequestBody<'_>>,
        out: &mut repe::structs::ResponseBody<'_>,
    ) -> repe::structs::StructResult<()> {
        match segments {
            [] => {
                out.write(self);
                Ok(())
            }
            ["note"] => {
                out.write(&self.note);
                Ok(())
            }
            _ => Err(repe::structs::StructError::InvalidPath {
                path: repe::structs::path_from_segments(segments),
            }),
        }
    }
}

/// `tally` before `manual`, for the same reason `Bench` orders its fields.
#[derive(Default, RepeStruct)]
struct Console {
    #[repe(nested)]
    tally: Tally,
    #[repe(nested)]
    manual: Manual,
}
structio::object!(Console { tally, manual });

#[test]
fn a_hand_written_child_declines_its_parent_s_listing_before_anything_runs() {
    // A hand-written child that never overrides `repe_shared_into` declines every
    // shared read, and it declines *late* — after the parent has already listed
    // the siblings before it. `repe_listing_declines` defaults to `true` so the
    // parent asks and gives up first, rather than discovering it partway.
    for (kind, router) in [
        (
            "Mutex",
            Router::new()
                .with_struct_shared::<Console, _>("/c", Arc::new(Mutex::new(Console::default()))),
        ),
        (
            "RwLock",
            Router::new()
                .with_struct_shared::<Console, _>("/c", Arc::new(RwLock::new(Console::default()))),
        ),
    ] {
        let message = answer(&router, &read("/c"));
        let value = body_text(&message);
        assert!(
            value.contains(r#""count":1"#),
            "under a {kind} a getter ran twice because a hand-written sibling \
             declined after it"
        );
    }
}

fn bench() -> Bench {
    Bench {
        site: String::from("lab-3"),
        inst: instrument(),
        tally: Tally::default(),
        counter: Counter::default(),
    }
}

#[test]
fn a_declining_sibling_does_not_make_an_earlier_getter_run_twice() {
    // The invariant `a_listing_invokes_each_getter_exactly_once` pins for one
    // struct, held across nesting — which is where it is easy to lose, because
    // the guard that enforces it is emitted per struct while the hazard spans
    // the whole subtree.
    //
    // `/bench` lists `tally` before `counter`. `Tally::count` is a `&self` getter
    // over a read counter, and `Tally`'s own listing is served shared — so a
    // listing that served `tally` and only then found `counter` declining would
    // rewind the bytes, retry exclusively, and call `count` a second time. The
    // first read of the object would report 2. The whole listing has to decline
    // before `tally` is ever reached.
    //
    // The body is identical either way, so nothing but the count catches this.
    for (kind, router) in [
        (
            "Mutex",
            Router::new().with_struct_shared::<Bench, _>("/bench", Arc::new(Mutex::new(bench()))),
        ),
        (
            "RwLock",
            Router::new().with_struct_shared::<Bench, _>("/bench", Arc::new(RwLock::new(bench()))),
        ),
    ] {
        let message = answer(&router, &read("/bench"));
        let value = body_text(&message);
        assert!(
            value.contains(r#""count":1"#),
            "under a {kind} a nested getter ran twice for one listing read"
        );
    }
}

#[test]
fn a_child_s_reading_getters_leave_its_ancestor_s_listing_shared() {
    let state = Arc::new(RwLock::new(bench()));
    let router = Router::new().with_struct_shared::<Bench, _>("/bench", Arc::clone(&state));

    // `/bench/inst` nests only `&self` getters, so the parent of *that* subtree
    // is served shared.
    let held = state.read().expect("the lock is not poisoned");
    let frame = try_off_thread(Duration::from_secs(10), {
        let router = router.clone();
        move || router.call(&read("/bench/inst"))
    })
    .expect("a nested listing with nothing that can decline must not wait")
    .expect("a non-notify request is answered");
    assert!(!Message::from_slice(&frame).unwrap().is_error());

    // `/bench` itself nests `Counter`, whose getter is `&mut self`, so the whole
    // listing still declines: the decline is transitive, and has to be.
    let (tx, rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let _ = tx.send(router.call(&read("/bench")));
    });
    assert!(
        rx.recv_timeout(Duration::from_millis(200)).is_err(),
        "a child that can decline still declines its ancestor's listing"
    );
    drop(held);
    rx.recv_timeout(Duration::from_secs(10))
        .expect("the listing completes once the guard is released");
    worker.join().expect("the worker thread finishes");
}

#[test]
fn a_call_below_a_nested_child_answers_the_same_either_way() {
    // A nested child is handed the body *borrow*, so the obligation to leave it
    // alone when declining is one level deeper than the arm that states it. This
    // is where a child that took the body and then declined would show: the
    // exclusive retry would see `None` and answer `BodyExpected` instead.
    let exclusive =
        Router::new().with_struct_shared::<Bench, _>("/bench", Arc::new(Mutex::new(bench())));
    let shared =
        Router::new().with_struct_shared::<Bench, _>("/bench", Arc::new(RwLock::new(bench())));

    for (name, request) in [
        // `&self` with arguments, two levels down: served shared.
        ("nested scale", write("/bench/inst/scale", "2.0")),
        // `&mut self` with arguments: the child declines, and the exclusive
        // retry has to receive the same body.
        ("nested calibrate", write("/bench/inst/calibrate", "9.0")),
        // A field write below the child, for the same reason.
        ("nested field write", write("/bench/inst/channel", "11")),
    ] {
        assert_eq!(
            exclusive.call(&request),
            shared.call(&request),
            "`{name}` must answer the same whichever guard served it"
        );
    }

    // And the mutation actually landed, rather than the declined attempt eating
    // the body on the way through.
    for (kind, router) in [("Mutex", exclusive), ("RwLock", shared)] {
        assert_eq!(
            answer(&router, &read("/bench/inst/channel"))
                .json_body::<u32>()
                .unwrap(),
            11,
            "under a {kind} the write below the nested child took effect"
        );
    }
}

#[test]
fn a_bench_listing_answers_the_same_either_way() {
    // The oracle for the fixture above: the declining listing must still produce
    // the exclusive path's frame exactly.
    let exclusive =
        Router::new().with_struct_shared::<Bench, _>("/bench", Arc::new(Mutex::new(bench())));
    let shared =
        Router::new().with_struct_shared::<Bench, _>("/bench", Arc::new(RwLock::new(bench())));

    for path in [
        "/bench",
        "/bench/inst",
        "/bench/tally",
        "/bench/counter",
        "/bench/site",
    ] {
        let request = read(path);
        assert_eq!(
            exclusive.call(&request),
            shared.call(&request),
            "reading `{path}` must answer the same whichever guard served it"
        );
    }
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
    assert_eq!(body_text(&message), r#"{"ticks":990,"source":"gps"}"#);
}

// ---------------------------------------------------------------------------
// The remaining published read shapes, against the same oracle
// ---------------------------------------------------------------------------

/// The struct-attribute method list, the other way a method reaches the wire.
/// Its receivers are declared by hand rather than read off a signature, and the
/// read path is what acts on that declaration.
#[derive(Clone, Default, RepeStruct)]
#[repe(methods(
    describe(&self) -> String,
    bump(&mut self) -> u32,
    scale(&self, factor: f64) -> f64
))]
struct Listed {
    gain: f64,
}
structio::object!(Listed { gain });

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

#[derive(Clone, Default, RepeStruct)]
struct Bay {
    slot: u32,
    #[repe(nested)]
    sensor: Faulty,
}
structio::object!(Bay { slot, sensor });

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

/// `REPE_SHARED_SERVES_BODIES`: whether the router should attempt the shared
/// borrow at all for a frame that carries a body.
///
/// REPE puts a body on a write and on a call with arguments alike, so the frame
/// cannot tell the two apart — but the type can, and the router reads this
/// before it takes the read lock. `false` is the whole point: a plain struct's
/// every write needs `&mut self`, so the lock, the walk and the decline are
/// work whose outcome is known before it starts.
///
/// Getting it wrong cannot change an answer, only concurrency: everything the
/// shared borrow serves with a body, the exclusive path serves identically.
/// That is what makes `true` the safe default for a hand-written impl, and it
/// is why these are pinned here — a shape that silently flips to `true` costs
/// throughput, and one that flips to `false` costs the concurrency this whole
/// path exists for, and neither shows up as a failing dispatch test.
mod shared_serves_bodies {
    use super::*;

    #[derive(Default, RepeStruct)]
    struct Plain {
        a: u64,
    }
    structio::object!(Plain { a });

    #[derive(Default, RepeStruct)]
    struct ReadonlyField {
        a: u64,
        #[repe(readonly)]
        ro: u64,
    }
    structio::object!(ReadonlyField { a, ro });

    #[derive(Default, RepeStruct)]
    #[repe(no_replace)]
    struct NoReplaceStruct {
        a: u64,
    }
    structio::object!(NoReplaceStruct { a });

    #[derive(Default, RepeStruct)]
    struct NestsPlain {
        #[repe(nested)]
        child: Plain,
    }
    structio::object!(NestsPlain { child });

    #[derive(Default, RepeStruct)]
    struct NestsReadonly {
        #[repe(nested)]
        child: ReadonlyField,
    }
    structio::object!(NestsReadonly { child });

    #[derive(Default, RepeStruct)]
    struct OptionalPlain {
        #[repe(nested)]
        child: Option<Plain>,
    }
    structio::object!(OptionalPlain { child });

    #[derive(Default, RepeStruct)]
    struct OptionalReadonly {
        #[repe(nested)]
        child: Option<ReadonlyField>,
    }
    structio::object!(OptionalReadonly { child });

    #[derive(Default, RepeStruct)]
    #[repe(methods)]
    struct ExclusiveMethod {
        a: u64,
    }
    structio::object!(ExclusiveMethod { a });

    #[repe::methods]
    impl ExclusiveMethod {
        fn bump(&mut self) -> u64 {
            self.a += 1;
            self.a
        }
    }

    #[derive(Default, RepeStruct)]
    #[repe(methods)]
    struct SharedCallWithArgs {
        a: u64,
    }
    structio::object!(SharedCallWithArgs { a });

    #[repe::methods]
    impl SharedCallWithArgs {
        fn add(&self, x: u64) -> u64 {
            self.a + x
        }
    }

    #[derive(Default, RepeStruct)]
    #[repe(methods)]
    struct SharedCallNoArgs {
        a: u64,
    }
    structio::object!(SharedCallNoArgs { a });

    #[repe::methods]
    impl SharedCallNoArgs {
        fn peek(&self) -> u64 {
            self.a
        }
    }

    #[derive(Default, RepeStruct)]
    #[repe(methods(probe(&self, x: u64) -> u64))]
    struct StructListedCall {
        a: u64,
    }
    structio::object!(StructListedCall { a });

    impl StructListedCall {
        fn probe(&self, x: u64) -> u64 {
            self.a + x
        }
    }

    fn serves<T: RepeStruct>() -> bool {
        T::REPE_SHARED_SERVES_BODIES
    }

    #[test]
    fn only_the_shapes_that_can_answer_a_body_ask_for_the_read_lock() {
        // Nothing here can answer a body without `&mut self`, so the router
        // skips the attempt. This is the shape the skip exists for.
        assert!(
            !serves::<Plain>(),
            "a plain struct's every write is exclusive"
        );
        assert!(
            !serves::<ExclusiveMethod>(),
            "a `&mut self` method cannot be called through a shared borrow"
        );
        assert!(
            !serves::<SharedCallNoArgs>(),
            "a `&self` method taking no arguments is reached by a bodiless frame"
        );

        // A refusal needs no state, so there is no reason to take the write
        // guard to give one.
        assert!(
            serves::<ReadonlyField>(),
            "a readonly field refuses a write under the shared borrow"
        );
        assert!(
            serves::<NoReplaceStruct>(),
            "a `no_replace` struct refuses a whole-object write under it too"
        );

        // A call, not a mutation: the reason the receiver decides.
        assert!(
            serves::<SharedCallWithArgs>(),
            "a `&self` method taking arguments is served shared, body and all"
        );
        assert!(
            serves::<StructListedCall>(),
            "and so is one listed on the struct rather than in an impl block"
        );

        // The answer composes down the tree, through `Option` as well as
        // through a plain child, or a parent would give up a child's shared
        // service without knowing it.
        assert!(!serves::<NestsPlain>());
        assert!(
            serves::<NestsReadonly>(),
            "a child that can answer makes its parent able to"
        );
        assert!(!serves::<OptionalPlain>());
        assert!(
            serves::<OptionalReadonly>(),
            "and `Option` forwards its child's answer rather than masking it"
        );
    }
}
