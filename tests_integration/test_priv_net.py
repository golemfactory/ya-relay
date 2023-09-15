from python_on_whales import docker
from utils import set_netem, Cluster, Client, Server


def test_sessions_within_network(compose_up):
    cluster: Cluster = compose_up(public_clients=2, alice_clients=2, bob_clients=0)

    check_session_after_ping_in_same_network(cluster, "public")
    check_session_after_ping_in_same_network(cluster, "alice")


def test_sessions_between_networks(compose_up):
    cluster: Cluster = compose_up(public_clients=1, alice_clients=1, bob_clients=1)
    server: Server = cluster.servers()[0]

    client_alice = cluster.clients("alice")[0]
    client_public = cluster.clients("public")[0]
    print(f"Testing session between 'alice' and 'public'")
    check_session_after_ping(server, client_1=client_alice, client_2=client_public)

    client_bob = cluster.clients("bob")[0]
    print(f"Testing session between 'bob' and 'alice'")
    check_session_after_ping(server, client_1=client_bob, client_2=client_alice)


def check_session_after_ping_in_same_network(cluster: Cluster, name_part: str):
    server: Server = cluster.servers()[0]
    clients = cluster.clients(name_part)
    client_1 = clients[0]
    client_2 = clients[1]
    print(f"Testing session within {name_part} net")
    check_session_after_ping(server, client_1, client_2)


def check_session_after_ping(server: Server, client_1: Client, client_2: Client):
    # Relay session with client will be visible using its public address
    client_2_address = client_2.address("public")

    sessions = client_1.sessions()
    print(f"Sessions before ping : {sessions}")
    sessions = {session["address"] for session in sessions["sessions"]}
    assert not {client_2_address}.issubset(sessions) or {client_2_address} == sessions

    client_1.ping(client_2.node_id)

    sessions = client_1.sessions()
    print(f"Sessions after ping: {sessions}")
    sessions = {session["address"] for session in sessions["sessions"]}
    server_and_client_2 = {server.address(), client_2_address}
    assert server_and_client_2.issubset(sessions) or server_and_client_2 == sessions
