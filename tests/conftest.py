import pytest
from python_on_whales import DockerClient
from python_on_whales import docker

@pytest.fixture(scope="session")
def docker_built():
    docker = DockerClient(compose_files=["../test_env/docker-compose.yml"])
    docker.compose.build()
    return docker

@pytest.fixture
def docker_up_down(docker_built):
    docker_built.compose.up(detach=True, wait=True)

    yield # wait for test to finish
    
    docker_built.compose.up(detach=True, wait=True)

def pumba():
    docker.run("gaiaadm/pumba")
    # TODO
