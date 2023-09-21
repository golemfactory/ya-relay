from python_on_whales import docker
from utils import set_netem, Cluster, Client, Server
import pytest
import logging
import time
import requests
from typing import Set, Any, Dict

LOGGER = logging.getLogger(__name__)


def test_sessions_with_and_without_p2p(compose_up):
    cluster: Cluster = compose_up(public_clients=2, alice_clients=2, bob_clients=2)

    LOGGER.info(f"Testing session between clients (same network, p2p)")
    server: Server = cluster.servers()[0]
    clients = cluster.clients("public")
    client_1 = clients[0]
    client_2 = clients[1]
    expected_sessions = {server.address(), client_2.address()}
    LOGGER.info(f"Testing session within public network")
    check_session_after_ping(client_1, client_2, expected_sessions)

    LOGGER.info(f"Testing session between clients (same network, no p2p)")
    server = cluster.servers()[0]
    clients = cluster.clients("alice")
    client_1 = clients[0]
    client_2 = clients[1]
    expected_sessions = {server.address()}
    LOGGER.info(f"Testing session within 'alice' network")
    check_session_after_ping(client_1, client_2, expected_sessions)

    LOGGER.info(f"Testing session between clients (different networks, no p2p)")
    server = cluster.servers()[0]
    client_1 = cluster.clients("alice")[0]
    client_2 = cluster.clients("bob")[0]
    expected_sessions = {server.address()}
    LOGGER.info(f"Testing session between 'alice' and 'bob' network")
    check_session_after_ping(client_1, client_2, expected_sessions)


def check_session_after_ping(
    client_1: Client,
    client_2: Client,
    expected_sessions: Set[str],
):
    client_1.ping(client_2.node_id)

    sessions = client_1.sessions()
    sessions = {session["address"] for session in sessions["sessions"]}
    assert expected_sessions == sessions


def test_session_expiration_after_disconnect(compose_up):
    session_expiration = 3
    cluster: Cluster = compose_up(
        public_clients=2, alice_clients=1, bob_clients=1, build_args={"SESSION_EXPIRATION": session_expiration}
    )
    server: Server = cluster.servers()[0]

    LOGGER.info("Testing session expiration on clients using Relayed connection")
    client_0 = cluster.clients("alice")[0]
    client_1 = cluster.clients("bob")[0]
    ping_response = client_0.ping(client_1.node_id)
    LOGGER.info("Check sessions after ping")
    assert client_1.node_id == ping_response["node_id"]
    check_sessions(client_0, {server.address()})
    check_sessions(client_1, {server.address()})
    LOGGER.info("Disconnecting bob")
    cluster.disconnect(client_1)
    time.sleep(1)
    LOGGER.info("Check sessions right after disconnect")
    check_sessions(client_0, {server.address()})
    time.sleep(5)
    LOGGER.info("Check sessions after session expiration period")
    check_sessions(client_0, {server.address()})

    LOGGER.info("Testing session expiration on clients using P2P connection")
    client_0 = cluster.clients("public")[0]
    client_1 = cluster.clients("public")[1]
    ping_response = client_0.ping(client_1.node_id)
    LOGGER.info("Check sessions after ping")
    assert client_1.node_id == ping_response["node_id"]
    check_sessions(client_0, {server.address(), client_1.address()})
    check_sessions(client_1, {server.address(), client_0.address()})
    LOGGER.info("Disconnecting p2p client")
    client_1_address = client_1.address()
    cluster.disconnect(client_1)
    time.sleep(1)
    LOGGER.info("Check sessions right after disconnect")
    check_sessions(client_0, {server.address(), client_1_address})
    time.sleep(5)
    LOGGER.info("Check sessions after session expiration period")
    check_sessions(client_0, {server.address()})


def test_server_session_restored_after_disconnect(compose_up):
    session_expiration = 3
    cluster: Cluster = compose_up(
        public_clients=1, alice_clients=0, bob_clients=0, build_args={"SESSION_EXPIRATION": session_expiration}
    )
    server: Server = cluster.servers()[0]

    LOGGER.info("Testing server session after start")
    client_0 = cluster.clients("public")[0]
    check_sessions(client_0, {server.address()})

    LOGGER.info("Testing server session is lost after disconnect")
    networks_0 = cluster.disconnect(client_0)
    time.sleep(5)
    cluster.connect(client_0, networks_0)
    check_sessions(client_0, {})

    LOGGER.info("Testing server session is restored")
    time.sleep(5)
    check_sessions(client_0, {server.address()})


def test_ping_after_disconnect_and_reconnect(compose_up):
    session_expiration = 3
    cluster: Cluster = compose_up(
        public_clients=0, alice_clients=1, bob_clients=1, build_args={"SESSION_EXPIRATION": session_expiration}
    )
    server: Server = cluster.servers()[0]

    LOGGER.info("Initial ping")
    alice = cluster.clients("alice")[0]
    bob = cluster.clients("bob")[0]
    bob_gateway = cluster.gateways("bob")[0]
    alice.ping(bob.node_id)

    LOGGER.info("Testing ping after disconnect should fail")
    networks = cluster.disconnect(bob)
    with pytest.raises(requests.exceptions.ReadTimeout) as excinfo:
        alice.ping(bob.node_id)
    assert "timeout" in str(excinfo.value)

    LOGGER.info("Testing sessions after disconnect")
    time.sleep(5)
    cluster.connect(bob, networks)
    bob.reset_gateway(bob_gateway.address("bob"))
    check_sessions(bob, {})

    LOGGER.info("Testing ping after reconnect")
    time.sleep(1)
    check_sessions(bob, {server.address()})

    try:
        alice.ping(bob.node_id)
    except Exception as excinfo:
        pytest.fail(f"Unexpected exception raised: {excinfo}")


def check_sessions(client: Client, expected_sessions: Set[Any] | Dict[Any, Any]):
    client_sessions = client.sessions()
    LOGGER.info(f"Sessions: {client_sessions}")
    actual_sessions = {session["address"] for session in client_sessions["sessions"]}
    assert len(expected_sessions) == 0 and len(actual_sessions) == 0 or expected_sessions == actual_sessions
