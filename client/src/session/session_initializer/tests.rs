use super::*;
use crate::config::ClientBuilder;
use crate::dispatch::Handler;
use crate::session::SessionLayer;
use futures::StreamExt;

async fn setup() -> (SessionInitializer, SessionLayer) {
    let mut config = ClientBuilder::from_url("udp://127.0.0.1:7477".parse().unwrap())
        .build_config()
        .await
        .unwrap();
    config.incoming_session_timeout = std::time::Duration::from_millis(20);
    let config = Arc::new(config);
    let layer = SessionLayer::new(config.clone());
    let (sink, mut rx) = futures::channel::mpsc::channel(1);
    tokio::task::spawn_local(async move { while rx.next().await.is_some() {} });
    let protocol = SessionInitializer::new(config, layer.clone(), sink);
    layer.state.lock().init_protocol = Some(protocol.clone());
    (protocol, layer)
}

fn hello(request_id: u64) -> proto::Request {
    proto::Request {
        request_id,
        kind: Some(proto::request::Session::default().into()),
    }
}

#[actix_rt::test]
async fn unknown_continuations_never_create_tasks_or_handshake_slots() {
    let (protocol, layer) = setup().await;
    let remote = "127.0.0.1:12345".parse().unwrap();
    for request_id in 0..10_000 {
        assert!(layer
            .clone()
            .on_request(SessionId::generate().to_vec(), hello(request_id), remote)
            .is_none());
    }
    let state = protocol.state.lock().unwrap();
    assert!(state.admitted.is_empty());
    assert!(state.incoming_sessions.is_empty());
    assert!(state.tmp_sessions.is_empty());
    assert!(state.handles.is_empty());
}

#[actix_rt::test]
async fn known_continuations_are_delivered_without_spawning_or_waiting() {
    let (protocol, layer) = setup().await;
    let id = SessionId::generate();
    let remote = "127.0.0.1:12345".parse().unwrap();
    let foreign = "127.0.0.1:12346".parse().unwrap();
    let (sender, mut receiver) = mpsc::channel(1);
    protocol
        .state
        .lock()
        .unwrap()
        .incoming_sessions
        .insert(id, IncomingSession { remote, sender });
    assert!(layer
        .clone()
        .on_request(id.to_vec(), hello(0), foreign)
        .is_none());
    assert!(receiver.try_recv().is_err());
    for request_id in 1..10_000 {
        assert!(layer
            .clone()
            .on_request(id.to_vec(), hello(request_id), remote)
            .is_none());
    }
    assert_eq!(receiver.try_recv().unwrap().0, 1);
    assert!(receiver.try_recv().is_err());
    assert!(protocol.state.lock().unwrap().admitted.is_empty());
}

#[actix_rt::test]
async fn repeated_hello_reserves_one_slot_before_spawning() {
    let (protocol, layer) = setup().await;
    let addr = "127.0.0.1:12345".parse().unwrap();
    let first = layer.clone().on_request(vec![], hello(0), addr).unwrap();
    for request_id in 1..10_000 {
        assert!(layer
            .clone()
            .on_request(vec![], hello(request_id), addr)
            .is_none());
    }
    {
        let state = protocol.state.lock().unwrap();
        assert_eq!(state.admitted.len(), 1);
        assert!(state.incoming_sessions.is_empty());
        assert!(state.handles.is_empty());
    }
    drop(first);
    assert!(protocol.state.lock().unwrap().admitted.is_empty());
    assert!(layer.on_request(vec![], hello(10_000), addr).is_some());
}

#[actix_rt::test]
async fn changing_ports_cannot_exceed_global_handshake_admission_limit() {
    let (protocol, layer) = setup().await;
    let mut pending = Vec::new();
    for port in 10000..11000 {
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        // Equal request IDs from distinct endpoints must not collide in deduplication.
        if let Some(task) = layer.clone().on_request(vec![], hello(1), addr) {
            pending.push(task);
        }
    }
    assert_eq!(pending.len(), MAX_INCOMING_HANDSHAKES);
    assert_eq!(
        protocol.state.lock().unwrap().admitted.len(),
        MAX_INCOMING_HANDSHAKES
    );
    drop(pending);
    assert!(protocol.state.lock().unwrap().admitted.is_empty());
}

#[actix_rt::test]
async fn temporary_session_is_reused_until_last_attempt_releases_it() {
    let (protocol, _) = setup().await;
    let addr = "127.0.0.1:12345".parse().unwrap();
    let first = protocol.lease_temporary_session(addr);
    let second = protocol.lease_temporary_session(addr);
    for _ in 0..10_000 {
        assert!(Arc::ptr_eq(
            &first.session,
            &protocol.temporary_session(&addr)
        ));
    }
    assert_eq!(protocol.state.lock().unwrap().tmp_sessions.len(), 1);
    drop(first);
    assert!(protocol.get_temporary_session(&addr).is_some());
    drop(second);
    assert!(protocol.get_temporary_session(&addr).is_none());
}

#[actix_rt::test]
async fn handshake_queue_is_bounded_and_bound_to_source() {
    let (protocol, _) = setup().await;
    let id = SessionId::generate();
    let remote = "127.0.0.1:12345".parse().unwrap();
    let foreign = "127.0.0.1:12346".parse().unwrap();
    let (sender, mut receiver) = mpsc::channel(1);
    let (handle, _) = AbortHandle::new_pair();
    {
        let mut state = protocol.state.lock().unwrap();
        state
            .incoming_sessions
            .insert(id, IncomingSession { remote, sender });
        state.handles.insert(id, handle);
    }
    let guard = IncomingSessionGuard {
        state: protocol.state.clone(),
        id,
    };
    assert!(protocol
        .try_continue_session(id, 0, foreign, Default::default())
        .is_err());
    assert!(receiver.try_recv().is_err());
    protocol
        .try_continue_session(id, 1, remote, Default::default())
        .unwrap();
    for request_id in 2..10_000 {
        assert!(protocol
            .try_continue_session(id, request_id, remote, Default::default())
            .is_err());
    }
    assert_eq!(receiver.try_recv().unwrap().0, 1);
    assert!(receiver.try_recv().is_err());
    drop(guard);
    let state = protocol.state.lock().unwrap();
    assert!(state.incoming_sessions.is_empty());
    assert!(state.handles.is_empty());
}

#[actix_rt::test]
async fn malformed_session_id_does_not_reserve_a_handshake_slot() {
    let (protocol, layer) = setup().await;
    let addr = "127.0.0.1:12345".parse().unwrap();
    for len in [1, 15, 17, 128] {
        assert!(layer
            .clone()
            .on_request(vec![0; len], hello(len as u64), addr)
            .is_none());
    }
    assert!(protocol.state.lock().unwrap().admitted.is_empty());
}

#[actix_rt::test]
async fn repeated_hello_does_not_extend_deadline_or_leave_handles() {
    let (protocol, layer) = setup().await;
    let addr = "127.0.0.1:12345".parse().unwrap();
    let (request, _) = protocol.prepare_session_request(false).await.unwrap();
    let first = layer
        .clone()
        .on_request(
            vec![],
            proto::Request {
                request_id: 0,
                kind: Some(request.clone().into()),
            },
            addr,
        )
        .unwrap();
    let task = tokio::task::spawn_local(first);
    // Let the admitted task publish its challenge and wait for a response.
    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    assert_eq!(protocol.state.lock().unwrap().incoming_sessions.len(), 1);
    for request_id in 1..10_000 {
        assert!(layer
            .clone()
            .on_request(
                vec![],
                proto::Request {
                    request_id,
                    kind: Some(request.clone().into()),
                },
                addr
            )
            .is_none());
    }
    tokio::time::timeout(std::time::Duration::from_millis(100), task)
        .await
        .unwrap()
        .unwrap();
    let state = protocol.state.lock().unwrap();
    assert!(state.admitted.is_empty());
    assert!(state.incoming_sessions.is_empty());
    assert!(state.tmp_sessions.is_empty());
    assert!(state.handles.is_empty());
}

#[actix_rt::test]
async fn cancelling_an_active_handshake_releases_all_owned_slots() {
    let (protocol, layer) = setup().await;
    let addr = "127.0.0.1:12345".parse().unwrap();
    let (request, _) = protocol.prepare_session_request(false).await.unwrap();
    let task = tokio::task::spawn_local(
        layer
            .on_request(
                vec![],
                proto::Request {
                    request_id: 0,
                    kind: Some(request.into()),
                },
                addr,
            )
            .unwrap(),
    );
    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    assert_eq!(protocol.state.lock().unwrap().incoming_sessions.len(), 1);
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let state = protocol.state.lock().unwrap();
    assert!(state.admitted.is_empty());
    assert!(state.incoming_sessions.is_empty());
    assert!(state.tmp_sessions.is_empty());
    assert!(state.handles.is_empty());
}
