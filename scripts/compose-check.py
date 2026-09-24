"""The compose files keep the worker where the design puts it.

Run from the repository root:  python3 scripts/compose-check.py

`recall-worker` holds a device key that can claim every conflict, and the
`claude` login merges run on. It is safe beside the server only while it is
unreachable and separate: no published or exposed port, no ingress labels,
no ingress network, no volume any other service mounts, and nothing that
reaches past its own container (the Docker socket, `privileged`, the host's
process or network namespace). It is also opt-in, behind the `worker`
profile, and reads its server from `RECALL_WORKER_SERVER`, never the
client's `RECALL_URL`. Each of those is one line in a YAML file that a later
edit could add without anyone noticing, so each is asserted here, for both
compose files, the way wrangler-check.py asserts the installer's config.

The checks are also run against copies of each file with one of those lines
added, and each copy must fail: a check that can never fail is not one.

Reads the files with PyYAML when it is installed, and otherwise through
`docker compose config`, which CI's runner has either way.
"""
import copy
import json
import os
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
FILES = {
    # file: the network its ingress reaches the server on
    'deploy/docker-compose.yml': 'tunnel',
    'deploy/docker-compose.traefik.yml': 'traefik',
}
SERVER, WORKER, BACKEND = 'recall-server', 'recall-worker', 'backend'
PROFILE = 'worker'
# Each of these reaches past the worker's own container.
ESCAPES = ('privileged', 'pid', 'ipc', 'network_mode', 'cap_add', 'devices')


def load(path):
    try:
        import yaml  # noqa: PLC0415
    except ImportError:
        yaml = None
    if yaml is not None:
        with open(os.path.join(ROOT, path)) as f:
            return yaml.safe_load(f)
    # The worker's profile is named, so a compose that leaves out services
    # whose profile is not active still shows the worker.
    out = subprocess.run(
        ['docker', 'compose', '-f', os.path.join(ROOT, path), '--profile', PROFILE,
         'config', '--format', 'json', '--no-interpolate'],
        check=True, capture_output=True, text=True,
    )
    return json.loads(out.stdout)


def names(value):
    """A list or a mapping of names, as compose accepts either."""
    if not value:
        return set()
    if isinstance(value, dict):
        return set(value)
    return set(value)


def volume_sources(service):
    """The named volumes and host paths a service mounts."""
    out = set()
    for v in service.get('volumes') or []:
        if isinstance(v, dict):
            out.add(v.get('source'))
        else:
            out.add(str(v).split(':', 1)[0])
    return out


def environment(service):
    """A service's environment as a mapping, whichever form it is written in."""
    env = service.get('environment') or {}
    if isinstance(env, list):
        env = dict(e.split('=', 1) if '=' in e else (e, None) for e in env)
    return env


def problems(doc, ingress):
    """Everything wrong with one compose file; empty when it is right."""
    found = []
    services = doc.get('services') or {}
    server, worker = services.get(SERVER), services.get(WORKER)
    if server is None or worker is None:
        return [f'both {SERVER} and {WORKER} are services']

    for key in ('ports', 'expose', 'labels'):
        if worker.get(key):
            found.append(f'{WORKER} has {key}; it must open nothing')
    for key in ESCAPES:
        if worker.get(key):
            found.append(f'{WORKER} has {key}; it must stay inside its own container')
    if any('docker.sock' in str(v) for v in volume_sources(worker)):
        found.append(f'{WORKER} mounts the Docker socket, which is root on the host')
    if worker.get('init') is not True:
        found.append(f'{WORKER} runs with init: true, so the CLI processes it starts are reaped')
    if list(worker.get('profiles') or []) != [PROFILE]:
        found.append(f'{WORKER} is opt-in: profiles: ["{PROFILE}"], and no other')
    build = worker.get('build') or {}
    if not isinstance(build, dict) or build.get('target') != 'worker':
        found.append(f'{WORKER} is built from the Dockerfile\'s worker target')
    env = environment(worker)
    if env.get('RECALL_WORKER_SERVER') != f'http://{SERVER}:8787':
        found.append(f'{WORKER} reaches the server directly: '
                     f'RECALL_WORKER_SERVER is http://{SERVER}:8787')
    if 'RECALL_URL' in env:
        found.append(f'{WORKER} is not given RECALL_URL, the client\'s variable; '
                     'it reads RECALL_WORKER_SERVER only')

    worker_nets = names(worker.get('networks'))
    if worker_nets != {BACKEND}:
        found.append(f'{WORKER} is on {BACKEND} and no other network, not {sorted(worker_nets)}')
    if BACKEND not in names(server.get('networks')):
        found.append(f'{SERVER} is on {BACKEND}')
    others = sorted(n for n, s in services.items()
                    if n not in (SERVER, WORKER) and BACKEND in names(s.get('networks')))
    if others:
        found.append(f'only {SERVER} and {WORKER} are on {BACKEND}, not {others}')
    backend = (doc.get('networks') or {}).get(BACKEND) or {}
    if backend.get('external') or backend.get('name') == ingress:
        found.append(f'{BACKEND} is this file\'s own network, not the ingress\'s')

    shared = volume_sources(worker) & volume_sources(server)
    if shared:
        found.append(f'{WORKER} shares no volume with {SERVER}, not {sorted(shared)}')
    # The worker's own volume holds its key and the login: nothing else
    # mounts it, read-only or not.
    for name, other in services.items():
        if name == WORKER:
            continue
        mounted = volume_sources(worker) & volume_sources(other)
        if mounted:
            found.append(f'{name} mounts none of {WORKER}\'s volumes, not {sorted(mounted)}')

    if server.get('ports'):
        found.append(f'{SERVER} uses expose, never ports')
    return found


def mutations(doc, ingress):
    """Copies of `doc`, each with one thing the checks must refuse."""
    def changed(label, change):
        d = copy.deepcopy(doc)
        change(d['services'][WORKER], d)
        return label, d

    def add_network(name):
        def change(w, _):
            nets = w.get('networks') or []
            if isinstance(nets, dict):
                nets[name] = None
            else:
                nets.append(name)
            w['networks'] = nets
        return change

    def share_volume(w, d):
        w.setdefault('volumes', []).append('recall-data:/server-data')

    def another_on_backend(_, d):
        add_network(BACKEND)(d['services']['sqlite-web'], d)

    def another_mounts_worker_volume(w, d):
        d['services']['sqlite-web'].setdefault('volumes', []).append(
            f'{sorted(volume_sources(w))[0]}:/worker:ro')

    def docker_socket(w, _):
        w.setdefault('volumes', []).append('/var/run/docker.sock:/var/run/docker.sock')

    def set_env(key, value):
        def change(w, _):
            env = environment(w)
            if value is None:
                env.pop(key, None)
            else:
                env[key] = value
            w['environment'] = env
        return change

    return [
        changed('a published port', lambda w, _: w.__setitem__('ports', ['127.0.0.1:9000:9000'])),
        changed('an exposed port', lambda w, _: w.__setitem__('expose', ['8787'])),
        changed('a Traefik label', lambda w, _: w.__setitem__('labels', {'traefik.enable': 'true'})),
        changed('the ingress network', add_network(ingress)),
        changed('the server\'s volume', share_volume),
        changed('another service on backend', another_on_backend),
        changed('another service mounting the worker\'s volume', another_mounts_worker_volume),
        changed('the Docker socket', docker_socket),
        changed('privileged', lambda w, _: w.__setitem__('privileged', True)),
        changed('the host\'s process namespace', lambda w, _: w.__setitem__('pid', 'host')),
        changed('the host\'s network', lambda w, _: w.__setitem__('network_mode', 'host')),
        changed('no init', lambda w, _: w.pop('init')),
        changed('no profile', lambda w, _: w.pop('profiles')),
        changed('another profile', lambda w, _: w.__setitem__('profiles', ['worker', 'default'])),
        changed('the client\'s RECALL_URL', set_env('RECALL_URL', 'https://recall.example.com')),
        changed('no RECALL_WORKER_SERVER', set_env('RECALL_WORKER_SERVER', None)),
        changed('the public server', set_env('RECALL_WORKER_SERVER', 'https://recall.example.com')),
        changed('the server image', lambda w, _: w.__setitem__('build', {'context': '..'})),
    ]


def main():
    fails = 0
    for path, ingress in FILES.items():
        doc = load(path)
        found = problems(doc, ingress)
        for p in found:
            print(f'  FAIL {path}: {p}')
        if not found:
            print(f'  ok   {path}')
        fails += len(found)
        for label, bad in mutations(doc, ingress):
            if problems(bad, ingress):
                print(f'  ok   {path} with {label} is refused')
            else:
                print(f'  FAIL {path} with {label} passes the checks')
                fails += 1
    print('all checks passed' if fails == 0 else f'{fails} FAILED')
    return 1 if fails else 0


if __name__ == '__main__':
    sys.exit(main())
