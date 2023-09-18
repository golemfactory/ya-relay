from python_on_whales import docker
from utils import set_netem, Cluster, Client, Server
import pytest
import logging
import time
from typing import Set, List

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

    client_alice = cluster.clients("alice")[0]
    client_bob = cluster.clients("bob")[0]
    LOGGER.info("Testing session expiration on clients using relayed connection")
    check_session_expiration_after_disconnect(cluster, client_alice, client_bob, session_expiration)

    client_public_1 = cluster.clients("public")[0]
    client_pubic_2 = cluster.clients("public")[1]
    LOGGER.info("Testing session expiration on clients using p2p connection")
    check_session_expiration_after_disconnect(cluster, client_public_1, client_pubic_2, session_expiration)


def check_session_expiration_after_disconnect(
    cluster: Cluster, client_1: Client, client_2: Client, session_expiration: int, ping_timeout: int = 13
):
    ping_response = client_1.ping(client_2.node_id)
    assert client_2.node_id == ping_response["node_id"]

    LOGGER.info("Disconnecting client")
    disconnect_time = time.time()
    cluster.disconnect(client_2)

    try:
        # Ping with reliable transport should fail on disconnected client which uses reliable transport.
        # It should happen after SESSION_EXPIRATION and before ping request timeout.
        ping_response = client_1.ping(client_2.node_id, timeout=ping_timeout, transport="reliable")
        pytest.fail(f"Ping to disconnected relayed client should fail. Instead got: {ping_response}")
    except BaseException as error:
        LOGGER.info(f"Ping error: {error}")
        ping_error_delay = time.time() - disconnect_time
        assert session_expiration < ping_error_delay and ping_timeout > ping_error_delay
