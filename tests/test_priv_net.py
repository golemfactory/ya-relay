from python_on_whales import docker
from utils import set_netem, Cluster, Client, Server
import pytest


def test_sessions_within_network(compose_up):
    cluster: Cluster = compose_up(public_clients=2, alice_clients=2, bob_clients=0)

    check_session_after_ping_in_same_network(cluster, "public")
    check_session_after_ping_in_same_network(cluster, "alice")


@pytest.mark.skip(reason="Binding to priv net does not work.")
def test_sessions_between_networks(compose_up):
    cluster: Cluster = compose_up(
        public_clients=1,
        alice_clients=1,
        bob_clients=1,
        build_args={
            # alice shared (with public client and relay) network pattern
            "PUBLIC_BIND_IP_PATTERN": "^172\\.25\\.0\\.",
            # alice shared network pattern
            "ALICE_BIND_IP_PATTERN": "^172\\.25\\.0\\.",
            # bob shared (not with alice) network pattern
            "BOB_BIND_IP_PATTERN": "^172\\.26\\.0\\.",
        },
    )
    server: Server = cluster.servers()[0]

    client_alice = cluster.clients("alice")[0]
    client_public = cluster.clients("public")[0]
    print(f"Testing session between 'alice' and 'public'")
    check_session_after_ping(
        server, client_1=client_alice, client_2=client_public, client_2_net_part="shared_a", server_net_part="shared_a"
    )

    ## TODO: Fails with: "Timeout: Timeout (3s) elapsed when waiting for `ReverseConnection` handshake. deadline has elapsed"
    # client_bob = cluster.clients("bob")[0]
    # print(f"Testing session between 'bob' and 'alice'")
    # check_session_after_ping(server, client_1=client_bob, client_2=client_alice, client_2_net_part = "shared_b", server_net_part = "a_shared")


def check_session_after_ping_in_same_network(
    cluster: Cluster, name_part: str, client_2_net_part: str = "public", server_net_part: str = "public"
):
    server: Server = cluster.servers()[0]
    clients = cluster.clients(name_part)
    client_1 = clients[0]
    client_2 = clients[1]
    print(f"Testing session within {name_part} net")
    check_session_after_ping(server, client_1, client_2, client_2_net_part, server_net_part)


def check_session_after_ping(
    server: Server,
    client_1: Client,
    client_2: Client,
    client_2_net_part: str = "public",
    server_net_part: str = "public",
):
    # Relay session with client will be visible using its public address
    client_2_address = client_2.address(client_2_net_part)

    sessions = client_1.sessions()
    print(f"Sessions before ping : {sessions}")
    sessions = {session["address"] for session in sessions["sessions"]}
    assert not {client_2_address}.issubset(sessions) or {client_2_address} == sessions

    client_1.ping(client_2.node_id)

    sessions = client_1.sessions()
    print(f"Sessions after ping: {sessions}")
    sessions = {session["address"] for session in sessions["sessions"]}
    server_and_client_2 = {server.address(server_net_part), client_2_address}
    assert server_and_client_2.issubset(sessions) or server_and_client_2 == sessions
