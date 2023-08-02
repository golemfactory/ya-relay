from python_on_whales import docker
from utils import set_netem

def test_ping(compose_up):
    docker_compose = compose_up(2)
    containers = docker_compose.container.list()
    for container in containers:
        name = container.name
        print("Container: {}".format(name))
        set_netem(container, latency="100ms")
    assert True
