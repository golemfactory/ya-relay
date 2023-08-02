from python_on_whales import docker, DockerClient, Container
from dataclasses import dataclass
from datetime import datetime, timedelta

def set_netem(
        container,
        latency="0ms"
        ):
    print("Set netem for '{}', latency {}".format(container, latency))
    command = ['tc', 'qdisc', 'replace', 'dev', 'eth0', 'root', 'netem', 'delay', latency]
    docker.execute(
        container=container,
        command=command,
        privileged=True,
        )

def __read_node_id(container: Container) -> str|None:
    until = timedelta(seconds=10)
    for log in docker.logs(container, stream=True, until=until):
        if isinstance(log, str) and "CLIENT NODE ID: " in log:
            node_id = log.split("CLIENT NODE ID: ")[1].strip()
            return node_id
    return None

@dataclass
class Node:
    container: Container

@dataclass
class Server(Node):
    pass

@dataclass
class Client(Node):
    node_id: str

# @dataclass
class Cluster:
    def from_client(compose_client: DockerClient) -> Cluster:
        server = None
        clients = []
        for container in compose_client.container.list():
            if "client" in container.name:
                container.logs
