//! The one-live-subscriber notify slot behind
//! [`WasmClient::subscribe_notifies`](crate::wasm_client::WasmClient::subscribe_notifies).
//!
//! Split out of [`crate::wasm_client`] so it can be tested. That module is gated
//! on `target_arch = "wasm32"`, and its tests would need a wasm runner and a
//! connected `WebSocket`; nothing here needs either, so these tests run in the
//! ordinary host suite where a regression is actually noticed. The gate below
//! keeps the module out of non-test host builds, where it would be dead code.
//!
//! The native `WebSocketClient` keeps its own copy of this logic: it stores the
//! sender behind a `std::sync::Mutex` and hands out a tokio channel, neither of
//! which exists on wasm32. The rules the two must agree on are the ones tested
//! here.

use crate::error::AlreadySubscribed;
use crate::message::Message;
use futures_channel::mpsc;
use std::cell::RefCell;

/// Holds the one live notify subscriber, or `None` while nobody is listening.
///
/// A `RefCell` suffices because wasm32 is single-threaded. It is not free,
/// though: dropping a sender or sending on one wakes the receiver
/// synchronously, so every function here releases its borrow before doing
/// either. A consumer that resubscribes when woken would otherwise re-enter a
/// live borrow and panic.
pub(crate) type NotifySlot = RefCell<Option<mpsc::UnboundedSender<Message>>>;

/// Install a fresh subscriber, refusing to displace a live one.
///
/// A slot whose receiver was already dropped counts as empty and is replaced
/// silently.
pub(crate) fn subscribe(
    slot: &NotifySlot,
) -> Result<mpsc::UnboundedReceiver<Message>, AlreadySubscribed> {
    let mut slot = slot.borrow_mut();
    if let Some(existing) = slot.as_ref()
        && !existing.is_closed()
    {
        return Err(AlreadySubscribed);
    }
    let (tx, rx) = mpsc::unbounded();
    // Safe to drop the old sender under the guard, unlike in `unsubscribe`: we
    // only get here when the slot was empty or its receiver was already gone, so
    // the wake that `close_channel` fires has no live task to re-enter with.
    *slot = Some(tx);
    Ok(rx)
}

/// Drop the active subscription. Subsequent notifies are discarded until the
/// next [`subscribe`].
pub(crate) fn unsubscribe(slot: &NotifySlot) {
    // Taken out and dropped *after* the guard, not assigned over. Dropping the
    // last sender calls `close_channel`, which wakes the parked receiver
    // synchronously; `*slot.borrow_mut() = None` would run that wake with the
    // mutable borrow still held, and a consumer that resubscribes on wake would
    // panic on the re-entrant borrow. Unlike the drops in `subscribe` and
    // `route`, this one can reach a *live* receiver, so the wake is real.
    let previous = slot.borrow_mut().take();
    drop(previous);
}

/// Route `msg` to the notify subscriber if its `notify` header flag is set.
///
/// Returns `None` once the message has been routed (or dropped for want of a
/// subscriber). A non-notify message is handed straight back as `Some`, for the
/// caller to correlate against its pending-request map.
///
/// The flag is checked here rather than at the call site so that the ordering
/// rule -- notify before correlation -- is covered by the tests below. A notify
/// carries no reply correlation, so matching one against `pending` would either
/// hit nothing or, worse, steal a waiter that happens to share the id.
pub(crate) fn route_notify(slot: &NotifySlot, msg: Message) -> Option<Message> {
    if msg.header.notify == 0 {
        return Some(msg);
    }
    route(slot, msg);
    None
}

/// Hand one inbound notify to the subscriber, if there is one.
fn route(slot: &NotifySlot, notify: Message) {
    // Cloned out so the borrow ends before the send: `unbounded_send` wakes the
    // consumer, and a task that resubscribes on wake could otherwise re-enter
    // this `RefCell` while it is still borrowed. Under `spawn_local` the wake is
    // a microtask and cannot re-enter, but the receiver may be polled by any
    // waker the application supplies.
    let Some(sender) = slot.borrow().clone() else {
        // No subscriber. Dropping silently matches `WebSocketClient`: chunk-style
        // notifies arrive at high rate, so per-drop logging would be an avalanche.
        return;
    };

    if sender.unbounded_send(notify).is_err() {
        // Receiver gone; empty the slot so the next `subscribe` is not refused by
        // a corpse. `WebSocketClient` first checks the slot still holds the
        // channel that failed, because another thread can subscribe in that
        // window. Nothing can here: a `RefCell` is `!Sync`, and a send to a
        // dropped receiver wakes no one.
        *slot.borrow_mut() = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::BodyFormat;
    use futures_util::StreamExt;
    use std::cell::Cell;
    use std::sync::Arc;

    thread_local! {
        /// The slot [`ReentrantProbe`] pokes at when woken, and what it found.
        /// A thread local rather than a captured reference because
        /// [`std::task::Wake`] requires `Send + Sync`, which a `RefCell` is not;
        /// the wake fires synchronously on this same thread, so this reaches the
        /// slot under test.
        static PROBE_SLOT: NotifySlot = const { RefCell::new(None) };
        static PROBE_SAW_FREE_SLOT: Cell<Option<bool>> = const { Cell::new(None) };
    }

    /// Stands in for a consumer that resubscribes the moment its stream ends.
    struct ReentrantProbe;

    impl std::task::Wake for ReentrantProbe {
        fn wake(self: Arc<Self>) {
            let free = PROBE_SLOT.with(|slot| slot.try_borrow_mut().is_ok());
            PROBE_SAW_FREE_SLOT.with(|seen| seen.set(Some(free)));
        }
    }

    fn notify(query: &str) -> Message {
        frame(query, true)
    }

    fn response(query: &str) -> Message {
        frame(query, false)
    }

    fn frame(query: &str, notify: bool) -> Message {
        Message::builder()
            .query_str(query)
            .notify(notify)
            .body_format(BodyFormat::Json)
            .body_utf8("{}")
            .build()
    }

    #[test]
    fn a_second_subscribe_is_refused_while_the_first_receiver_lives() {
        let slot = NotifySlot::default();
        let _first = subscribe(&slot).expect("first subscribe");

        assert_eq!(subscribe(&slot).unwrap_err(), AlreadySubscribed);
    }

    #[test]
    fn the_refused_subscribe_leaves_the_incumbent_working() {
        // The point of refusing rather than replacing: a clone of the client
        // must not be able to silently steal the stream.
        let slot = NotifySlot::default();
        let mut first = subscribe(&slot).expect("first subscribe");
        let _ = subscribe(&slot);

        route(&slot, notify("/still-mine"));

        let received = first.try_recv().expect("a notify");
        assert_eq!(received.query_utf8(), "/still-mine");
    }

    #[test]
    fn dropping_the_receiver_frees_the_slot_for_a_new_subscriber() {
        let slot = NotifySlot::default();
        drop(subscribe(&slot).expect("first subscribe"));

        // Stale, not live: no `unsubscribe` needed.
        let mut second = subscribe(&slot).expect("resubscribe over a stale slot");
        route(&slot, notify("/second"));

        assert_eq!(second.try_recv().expect("a notify").query_utf8(), "/second");
    }

    #[test]
    fn unsubscribe_hands_the_slot_back() {
        let slot = NotifySlot::default();
        let _first = subscribe(&slot).expect("first subscribe");

        unsubscribe(&slot);

        subscribe(&slot).expect("subscribe after unsubscribe");
    }

    #[test]
    fn a_notify_with_no_subscriber_is_dropped_not_queued() {
        // Dropped for real: nothing installs a sender behind the scenes, and the
        // message is not buffered for whoever subscribes next. A subscriber that
        // attaches late starts from the notifies that follow it.
        let slot = NotifySlot::default();

        route(&slot, notify("/nobody-listening"));

        assert!(slot.borrow().is_none(), "no sender should be installed");
        let mut late = subscribe(&slot).expect("subscribe after the drop");
        assert!(
            late.try_recv().is_err(),
            "the earlier notify must not be replayed"
        );
    }

    #[test]
    fn routing_to_a_dropped_receiver_clears_the_stale_sender() {
        // Without this the slot would stay non-empty forever, and every later
        // `subscribe` would see a live-looking sender and return AlreadySubscribed.
        let slot = NotifySlot::default();
        drop(subscribe(&slot).expect("first subscribe"));

        route(&slot, notify("/into-the-void"));

        assert!(slot.borrow().is_none());
    }

    #[test]
    fn unsubscribe_releases_its_borrow_before_waking_the_receiver() {
        // Dropping the last sender calls `close_channel`, which wakes the parked
        // receiver synchronously. If that wake runs while the slot is still
        // mutably borrowed, a consumer that resubscribes on end-of-stream -- the
        // reconnect flow the docs prescribe -- panics on the re-entrant borrow.
        let mut rx = PROBE_SLOT.with(subscribe).expect("subscribe");

        // Park the receiver so there is a registered waker to fire.
        let waker = std::task::Waker::from(Arc::new(ReentrantProbe));
        let mut cx = std::task::Context::from_waker(&waker);
        assert!(
            rx.poll_next_unpin(&mut cx).is_pending(),
            "an empty, open channel should park"
        );

        PROBE_SLOT.with(unsubscribe);

        match PROBE_SAW_FREE_SLOT.with(|seen| seen.get()) {
            Some(true) => {}
            Some(false) => panic!("the slot was still borrowed when the receiver woke"),
            None => panic!("unsubscribe did not wake the parked receiver"),
        }
    }

    #[test]
    fn route_notify_consumes_a_notify_frame() {
        let slot = NotifySlot::default();
        let mut rx = subscribe(&slot).expect("subscribe");

        let passed_back = route_notify(&slot, notify("/pushed"));

        assert!(passed_back.is_none(), "a notify must not reach correlation");
        assert_eq!(rx.try_recv().expect("a notify").query_utf8(), "/pushed");
    }

    #[test]
    fn route_notify_hands_a_response_frame_back() {
        // The other half of the ordering rule: a frame without the flag belongs
        // to the pending-request map, and must not be siphoned into the stream.
        let slot = NotifySlot::default();
        let mut rx = subscribe(&slot).expect("subscribe");

        let passed_back = route_notify(&slot, response("/reply"));

        assert_eq!(
            passed_back.expect("response handed back").query_utf8(),
            "/reply"
        );
        assert!(rx.try_recv().is_err(), "subscriber must see nothing");
    }

    #[test]
    fn a_delivered_notify_leaves_the_subscription_intact() {
        // Only a *failed* send may empty the slot. Clearing on every send would
        // turn the subscription into a one-shot.
        let slot = NotifySlot::default();
        let mut rx = subscribe(&slot).expect("subscribe");

        route(&slot, notify("/first"));
        route(&slot, notify("/second"));

        assert_eq!(rx.try_recv().expect("a notify").query_utf8(), "/first");
        assert_eq!(rx.try_recv().expect("a notify").query_utf8(), "/second");
        assert!(slot.borrow().is_some());
    }
}
