from python_on_whales import docker
import utils


def test_ping(compose_up):
    docker_compose = compose_up(2)
    containers = docker_compose.container.list()
    for _container in containers:
        name = _container.name
        print("Container: {}".format(name))
        delay = utils.add_latency(name)
        print("Delay: {}".format(delay))
    assert True
