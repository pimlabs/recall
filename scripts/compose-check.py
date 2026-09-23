"""The compose files keep the worker where the design puts it.

Run from the repository root:  python3 scripts/compose-check.py

`recall-worker` holds a device key that can claim every conflict, and the
`claude` login merges run on. It is safe beside the server only while it is
unreachable and separate: no published or exposed port, no ingress labels,
no ingress network, and no volume shared with the server, whose database
and backups the internet-facing process holds. Each of those is one line in
a YAML file that a later edit could add without anyone noticing, so each is
asserted here, for both compose files, the way wrangler-check.py asserts
the installer's config.

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


def load(path):
    try:
        import yaml  # noqa: PLC0415
    except ImportError:
        yaml = None
    if yaml is not None:
        with open(os.path.join(ROOT, path)) as f:
            return yaml.safe_load(f)
    out = subprocess.run(
        ['docker', 'compose', '-f', os.path.join(ROOT, path), 'config',
         '--format', 'json', '--no-interpolate'],
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
    build = worker.get('build') or {}
    if not isinstance(build, dict) or build.get('target') != 'worker':
        found.append(f'{WORKER} is built from the Dockerfile\'s worker target')
    env = worker.get('environment') or {}
    if isinstance(env, list):
        env = dict(e.split('=', 1) for e in env if '=' in e)
    if env.get('RECALL_URL') != f'http://{SERVER}:8787':
        found.append(f'{WORKER} reaches the server directly, at http://{SERVER}:8787')

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

    return [
        changed('a published port', lambda w, _: w.__setitem__('ports', ['127.0.0.1:9000:9000'])),
        changed('an exposed port', lambda w, _: w.__setitem__('expose', ['8787'])),
        changed('a Traefik label', lambda w, _: w.__setitem__('labels', {'traefik.enable': 'true'})),
        changed('the ingress network', add_network(ingress)),
        changed('the server\'s volume', share_volume),
        changed('another service on backend', another_on_backend),
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
