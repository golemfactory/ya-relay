from python_on_whales import docker

def add_latency(
        containers,
        latency=100,
        duration="10s",
        detach=False
        ):
    command = ['netem', '--duration', duration, 'delay', '--time', latency, 'containers', containers]
    container = docker.run(
        image="gaiaadm/pumba",
        volumes=[("/var/run/docker.sock","/var/run/docker.sock")],
        detach=detach,
        remove=True,
        command=command,
    )

    return container
