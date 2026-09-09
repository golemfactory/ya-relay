use super::*;

#[test]
fn cancellation_before_poll_does_not_accumulate_response_slots() {
    let dispatcher = Dispatcher::default();
    for id in 0..10_000 {
        let pending = dispatcher.response::<proto::response::Pong>(id, Duration::from_secs(1));
        assert_eq!(dispatcher.responses.lock().unwrap().len(), 1);
        drop(pending);
        assert!(dispatcher.responses.lock().unwrap().is_empty());
    }
}

#[test]
fn cancelled_old_request_does_not_remove_replacement() {
    let dispatcher = Dispatcher::default();
    let old = dispatcher.response::<proto::response::Pong>(1, Duration::from_secs(1));
    let new = dispatcher.response::<proto::response::Pong>(1, Duration::from_secs(1));
    drop(old);
    assert_eq!(dispatcher.responses.lock().unwrap().len(), 1);
    drop(new);
    assert!(dispatcher.responses.lock().unwrap().is_empty());
}
