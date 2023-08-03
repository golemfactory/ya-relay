from python_on_whales import docker, DockerClient, Container
from datetime import datetime, timedelta
import json
from typing import TypeVar
import re
import requests

http_client_headers = { "Accept": "application/json" }

def set_netem(
        container,
        latency="0ms"
        ):
    print(f"Set netem for '{container.name}', latency {latency}")
    docker.execute(
        container=container,
        command=['tc', 'qdisc', 'replace', 'dev', 'eth0', 'root', 'netem', 'delay', latency],
        privileged=True,
        )

def read_node_id(container: Container) -> str:
    until = datetime.now() + timedelta(seconds=60)
    node_id_log = b'CLIENT NODE ID: '
    node_id = ""
    for _, log in docker.logs(container, until=until, stream=True, follow=True):
        if node_id_log in log:
            node_id = log.split(node_id_log)[1].strip().decode('utf-8')
            break
    assert re.match(r'0x[0-9a-fA-F]{40}', node_id), f"Invalid client Node Id {self.node_id} of container: {vars(container)}" 
    return node_id


def read_json_response(response: requests.Response):
    if response.status_code == 200:
        j = json.loads(response.content)
        print(f"Response: {j}")
        return j
    else:
        raise Exception(response.content)

class Node:
    container: Container
    ports: dict[str, int]

    def __init__(self, container: Container):
        self.container = container
        if container.network_settings.ports is not None:
            self.ports = {
                key: int(next(item['HostPort'] for item in value if item['HostIp'] == '0.0.0.0'))
                for key, value in self.container.network_settings.ports.items()
            }
        else:
            self.ports = {}

class Server(Node):
    
    def __init__(self, container: Container):
        Node.__init__(self, container=container)

TClient = TypeVar("TClient", bound="Client")

class Client(Node):
    node_id: str

    def __init__(self, container: Container):
        Node.__init__(self, container=container)
        self.node_id = read_node_id(container=container)

    def ping(self, node_id: str, port: int = 8081):
        port = self.ports[f"{port}/tcp"]
        response: requests.Response = requests.get(
            f"http://localhost:{port}/ping/{node_id}", 
            headers=http_client_headers
            )
        return read_json_response(response)
    
    def sessions(self, port: int = 8081):
        port = self.ports[f"{port}/tcp"]
        response: requests.Response = requests.get(
            f"http://localhost:{port}/sessions", 
            headers=http_client_headers
            )
        return read_json_response(response)


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
