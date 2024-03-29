from python_on_whales import docker
from utils import Cluster, Client, Server
import concurrent.futures
import pytest
import logging
import time
import requests
from typing import Set, Any, Dict

LOGGER = logging.getLogger(__name__)


def check_session_after_ping(
    client_1: Client,
    client_2: Client,
    expected_sessions: Set[str],
):
    client_1.ping(client_2.node_id)

    check_sessions(client_1, expected_sessions)


def check_sessions(client: Client, expected_sessions: Set[Any] | Dict[Any, Any]):
    client_sessions = client.sessions()
    actual_sessions = {session["address"] for session in client_sessions["sessions"]}
    try:
        assert len(expected_sessions) == 0 and len(actual_sessions) == 0 or expected_sessions == actual_sessions
    except AssertionError as e:
        LOGGER.info(f"Client logs for {client.container.name}\n{client.logs()}")
        raise e


def test_sessions_with_and_without_p2p(compose_up):
    cluster: Cluster = compose_up(
        public_clients=2, alice_clients=2, bob_clients=2, build_args={"RUST_LOG": "info,ya_relay_client::session=trace"}
    )

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


def test_session_expiration_after_disconnect_and_reinit_after_reconnect(compose_up):
    session_expiration = 3
    cluster: Cluster = compose_up(
        public_clients=2,
        alice_clients=1,
        bob_clients=1,
        build_args={"SESSION_EXPIRATION": session_expiration, "RUST_LOG": "info,ya_relay_client::session=trace"},
    )
    server: Server = cluster.servers()[0]

    LOGGER.info("Testing session expiration on clients using Relayed connection")
    client_0 = cluster.clients("alice")[0]
    client_1 = cluster.clients("bob")[0]
    client_1_gateway = cluster.gateways("bob")[0]
    ping_response = client_0.ping(client_1.node_id)
    LOGGER.info("Check sessions after ping")
    assert client_1.node_id == ping_response["node_id"]
    check_sessions(client_0, {server.address()})
    check_sessions(client_1, {server.address()})
    LOGGER.info("Disconnecting bob")
    client_1_networks = cluster.disconnect(client_1)
    time.sleep(1)
    LOGGER.info("Check sessions right after disconnect")
    check_sessions(client_0, {server.address()})
    time.sleep(5)
    LOGGER.info("Check sessions after session expiration period")
    check_sessions(client_0, {server.address()})
    check_sessions(client_1, {})
    LOGGER.info("Check server session was lost after disconnect but gets reconnected")
    cluster.connect(client_1, client_1_networks)
    client_1.reset_gateway(client_1_gateway.address("bob"))
    time.sleep(5)
    check_sessions(client_1, {server.address()})

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
    client_1_networks = cluster.disconnect(client_1)
    time.sleep(1)
    LOGGER.info("Check sessions right after disconnect")
    check_sessions(client_0, {server.address(), client_1_address})
    time.sleep(5)
    LOGGER.info("Check sessions after session expiration period")
    LOGGER.info(f"address: {client_0.address()} {client_1.address()} {server.address()}")
    check_sessions(client_0, {server.address()})

    LOGGER.info("Check server session was lost after disconnect but gets reconnected")
    cluster.connect(client_1, client_1_networks)
    check_sessions(client_1, {client_0.address()})
    time.sleep(5)
    check_sessions(client_1, {client_0.address(), server.address()})


def test_ping_after_disconnect_and_reconnect(compose_up):
    session_expiration = 3
    cluster: Cluster = compose_up(
        public_clients=0,
        alice_clients=1,
        bob_clients=1,
        build_args={"SESSION_EXPIRATION": session_expiration, "RUST_LOG": "info,ya_relay_client::session=trace"},
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

    LOGGER.info("Testing ping after reconnect")
    cluster.connect(bob, networks)
    bob.reset_gateway(bob_gateway.address("bob"))
    time.sleep(1)

    try:
        alice.ping(bob.node_id)
    except Exception as excinfo:
        LOGGER.info(f"Alice logs\n{alice.logs()}")
        LOGGER.info(f"Bob logs\n{bob.logs()}")
        pytest.fail(f"Unexpected exception raised: {excinfo}")


# For now, simultaneous session initialization prevents p2p connection as both nodes think they initiated
# the session and drop the incoming initialization packet with a log line:
# [DEBUG ya_relay_client::session::network_view] Initialization of session with: [0xXX] in progress, state: Outgoing-Initializing. Thread will wait for finish.
# When this fixed, the test should show a direct session between the two nodes, instead of the relayed connection.
def test_simultaneous_session_initialization(compose_up):
    cluster: Cluster = compose_up(
        public_clients=2,
        alice_clients=0,
        bob_clients=0,
        build_args={"CLIENT_LATENCY": "300ms", "RUST_LOG": "info,ya_relay_client::session=trace"},
    )
    server: Server = cluster.servers()[0]

    LOGGER.info("Testing simultaneous session initialization")
    client_0 = cluster.clients("public")[0]
    client_1 = cluster.clients("public")[1]

    with concurrent.futures.ThreadPoolExecutor(max_workers=2) as executor:
        executor.submit(client_0.ping, client_1.node_id)
        executor.submit(client_1.ping, client_0.node_id)

    check_sessions(client_0, {server.address()})
    # LOGGER.info(f"Client logs for {client_0.container.name}\n{client_0.logs()}")
    check_sessions(client_1, {server.address()})
    # LOGGER.info(f"Client logs for {client_1.container.name}\n{client_1.logs()}")
