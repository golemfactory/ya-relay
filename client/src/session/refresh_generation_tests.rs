use super::*;
use crate::config::ClientBuilder;
use futures::StreamExt;
use ya_relay_core::crypto::{SecretKey, SessionCrypto};

async fn setup() -> (SessionLayer, Arc<DirectSession>, NodeInfo) {
    let config = Arc::new(
        ClientBuilder::from_url("udp://127.0.0.1:7477".parse().unwrap())
            .build_config()
            .await
            .unwrap(),
    );
    let layer = SessionLayer::new(config.clone());
    let (sink, mut receiver) = futures::channel::mpsc::channel(1);
    tokio::task::spawn_local(async move { while receiver.next().await.is_some() {} });
    let relay = DirectSession::new_relay(
        NodeId::default(),
        RawSession::new(config.srv_addr, SessionId::generate(), sink),
    )
    .unwrap();
    layer
        .state
        .lock()
        .p2p_sessions
        .insert(config.srv_addr, relay.clone());
    let identity = Identity::from(SecretKey::from_raw(&[1; 32]).unwrap().public());
    let info = NodeInfo {
        identities: vec![identity.clone()],
        authenticated_identities: vec![identity.node_id],
        endpoints: vec![],
        slot: 17,
        supported_encryption: encryption::supported_encryptions(),
        session_key: Some(SessionCrypto::generate().unwrap().pub_key()),
    };
    (layer, relay, info)
}

async fn install(
    layer: &SessionLayer,
    relay: Arc<DirectSession>,
    info: NodeInfo,
) -> (NodeSnapshot, Arc<NodeRouting>, SessionPermit) {
    let id = info.default_node_id();
    layer.registry.update_entry(info.clone()).await.unwrap();
    let permit = match layer.registry.lock_outgoing(id, &[], layer.clone()).await {
        SessionLock::Permit(permit) => permit,
        _ => panic!("expected new generation"),
    };
    let owner = permit.registry.clone();
    let identities = NodeEntry {
        default_id: info.identities[0].clone(),
        identities: info.identities,
    };
    relay.register(identities.clone().into(), info.slot);
    let routing = NodeRouting::new(
        identities,
        relay,
        encryption::new(
            info.supported_encryption,
            info.session_key,
            layer.config.session_crypto.clone(),
        )
        .unwrap(),
        info.authenticated_identities,
        owner.generation(),
    );
    layer.register_routing(routing.clone()).await.unwrap();
    (owner, routing, permit)
}

#[actix_rt::test]
async fn delayed_refresh_cannot_mutate_reconnected_generation_or_aliases() {
    let (layer, relay, old_info) = setup().await;
    let id = old_info.default_node_id();
    let (old_owner, _original, _old_permit) =
        install(&layer, relay.clone(), old_info.clone()).await;
    let (started, start) = tokio::sync::oneshot::channel();
    let (reply, response) = tokio::sync::oneshot::channel();
    let refreshing = layer.clone();
    let task = tokio::task::spawn_local(async move {
        refreshing
            .refresh_node_encryption_with(id, async {
                started.send(()).unwrap();
                Ok(response.await.unwrap())
            })
            .await
    });
    // Hold the lookup response until a new generation has been installed.
    start.await.unwrap();
    layer.unregister_generation(old_owner.clone()).await;
    let mut new_info = old_info.clone();
    new_info.slot = 22;
    new_info.session_key = Some(SessionCrypto::generate().unwrap().pub_key());
    let (_, replacement, _new_permit) = install(&layer, relay.clone(), new_info.clone()).await;
    let mut stale_reply = old_info;
    let stale_alias = Identity::from(SecretKey::from_raw(&[2; 32]).unwrap().public());
    stale_reply.identities.push(stale_alias.clone());

    reply
        .send(stale_reply)
        .unwrap_or_else(|_| panic!("refresh stopped before receiving its response"));
    assert!(task.await.unwrap().is_err());
    let current = layer
        .registry
        .get_entry(id)
        .await
        .unwrap()
        .info()
        .await
        .just_get();
    assert_eq!(
        current.session_key.as_ref().map(|key| key.bytes()),
        new_info.session_key.as_ref().map(|key| key.bytes())
    );
    assert_eq!(current.slot, 22);
    assert!(layer
        .registry
        .get_entry(stale_alias.node_id)
        .await
        .is_none());
    assert!(relay.get_by_slot(17).is_none());
    assert!(relay.get_by_slot(22).is_some());
    assert!(Arc::ptr_eq(
        layer.state.lock().nodes.get(&id).unwrap(),
        &replacement
    ));
}

#[actix_rt::test]
async fn older_refresh_cannot_overwrite_newer_refresh_in_same_generation() {
    let (layer, relay, old_info) = setup().await;
    let id = old_info.default_node_id();
    let (owner, original, _permit) = install(&layer, relay.clone(), old_info.clone()).await;
    let mut new_info = old_info.clone();
    new_info.slot = 23;
    new_info.session_key = Some(SessionCrypto::generate().unwrap().pub_key());
    layer
        .publish_encryption_refresh(id, &owner, &original, new_info.clone())
        .await
        .unwrap();
    assert!(layer
        .publish_encryption_refresh(id, &owner, &original, old_info)
        .await
        .is_err());
    let current = layer
        .registry
        .get_entry(id)
        .await
        .unwrap()
        .info()
        .await
        .just_get();
    assert_eq!(
        current.session_key.as_ref().map(|key| key.bytes()),
        new_info.session_key.as_ref().map(|key| key.bytes())
    );
    assert_eq!(current.slot, 23);
    assert!(relay.get_by_slot(17).is_none());
    assert!(relay.get_by_slot(23).is_some());
}

#[actix_rt::test]
async fn physical_route_replacement_rejects_refresh_before_metadata_update() {
    let (layer, relay, info) = setup().await;
    let id = info.default_node_id();
    let (owner, original, _permit) = install(&layer, relay.clone(), info.clone()).await;
    let replacement = Arc::new((*relay).clone());
    layer
        .state
        .lock()
        .p2p_sessions
        .insert(relay.raw.remote, replacement);
    let mut reply = info.clone();
    reply.slot = 24;
    assert!(layer
        .publish_encryption_refresh(id, &owner, &original, reply)
        .await
        .is_err());
    assert_eq!(
        layer
            .registry
            .get_entry(id)
            .await
            .unwrap()
            .info()
            .await
            .just_get()
            .slot,
        17
    );
    assert!(relay.get_by_slot(24).is_none());
}
