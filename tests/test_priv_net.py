from python_on_whales import docker
from utils import set_netem, Cluster, Client, Server
import pytest
import logging
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
