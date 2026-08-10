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
/// A `RefCell` suffices because wasm32 is single-threaded and every borrow here
/// is released before anything that could re-enter.
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
    *slot = Some(tx);
    Ok(rx)
}

/// Drop the active subscription. Subsequent notifies are discarded until the
/// next [`subscribe`].
pub(crate) fn unsubscribe(slot: &NotifySlot) {
    *slot.borrow_mut() = None;
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
    fn routing_with_no_subscriber_is_a_silent_no_op() {
        let slot = NotifySlot::default();

        route(&slot, notify("/nobody-listening"));

        assert!(slot.borrow().is_none());
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
