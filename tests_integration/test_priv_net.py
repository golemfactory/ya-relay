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
    server: Server = cluster.servers()[0]

    LOGGER.info("Testing session expiration on clients using Relayed connection")
    client_alice = cluster.clients("alice")[0]
    client_bob = cluster.clients("bob")[0]
    ping_response = client_alice.ping(client_bob.node_id)
    LOGGER.info("Check sessions after ping")
    assert client_bob.node_id == ping_response["node_id"]
    check_sessions(client_alice, {server.address()})
    check_sessions(client_bob, {server.address()})
    LOGGER.info("Disconnecting bob")
    cluster.disconnect(client_bob)
    time.sleep(1)
    LOGGER.info("Check sessions right after disconnect")
    check_sessions(client_alice, {server.address()})
    # check_sessions(client_bob, {server.address()})
    time.sleep(2)
    LOGGER.info("Check sessions after session expiration period")
    check_sessions(client_alice, {server.address()})
    # check_sessions(client_bob, {})

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
    # check_sessions(client_1, {server.address(), client_0.address()})
    time.sleep(3)
    LOGGER.info("Check sessions after session expiration period")
    check_sessions(client_0, {server.address()})
    # check_sessions(client_1, {})

def check_sessions(client: Client, expected_sessions):
    sessions = client.sessions()
    assert expected_sessions == {session["address"] for session in sessions["sessions"]}
