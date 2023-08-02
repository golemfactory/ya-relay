from python_on_whales import docker

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
