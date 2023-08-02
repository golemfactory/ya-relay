from python_on_whales import docker, DockerClient, Container
from dataclasses import dataclass
from datetime import datetime, timedelta
import re

def set_netem(
        container,
        latency="0ms"
        ):
    print(f"Set netem for '{container.name}', latency {latency}")
    command = ['tc', 'qdisc', 'replace', 'dev', 'eth0', 'root', 'netem', 'delay', latency]
    docker.execute(
        container=container,
        command=command,
        privileged=True,
        )

@dataclass
class Node:
    container: Container

@dataclass
class Server(Node):
    pass

class Client(Node):
    node_id: str

    def __init__(self, container: Container):
        self.container=container
        until = datetime.now() + timedelta(seconds=60)
        node_id_log = b'CLIENT NODE ID: '
        for _, log in docker.logs(self.container, until=until, stream=True, follow=True):
            if node_id_log in log:
                self.node_id = log.split(node_id_log)[1].strip().decode('utf-8')
                break
        assert re.match(r'0x[0-9a-fA-F]{40}', self.node_id), f"Invalid client Node Id {self.node_id} of container: {vars(container)}"
        print("New client. Node Id {}".format(self.node_id))

class Cluster:
    docker_client: DockerClient
    clients: list[Client]
    servers: list[Server]

    def __init__(self, compose_client: DockerClient):
        self.clients = []
        self.servers = []
        for container in compose_client.container.list():
            if "client" in container.name:
                client = Client(container=container)
                self.clients.append(client)
            elif "relay_server" in container.name:
                server = Server(container=container)
                self.servers.append(server)
