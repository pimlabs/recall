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

Two more are asserted for all three compose files, direct TLS included,
because procedures in deploy/README.md depend on them: `recall-server` and
`sqlite-web` keep the container names `recall-server` and
`recall-sqlite-web`, which the restore stops and starts by name, needing no
`-f`; and `recall-server` has a `stop_grace_period` of at least 60 seconds,
so a stop lets a merge in flight finish and reaches the checkpoint that
empties the WAL into recall.db, rather than being killed at Docker's
default of 10. `recall-worker` keeps the container name `recall-worker` too,
in whichever of the two files defines it, since deploy/README.md's worker
procedures stop, restart and read its logs by that name the same way.

The checks are also run against copies of each file with one of those lines
added, and each copy must fail: a check that can never fail is not one.

Reads the files with PyYAML when it is installed, and otherwise through
`docker compose config`, which CI's runner has either way.

The same run also lints deploy/README.md and every docs/**/*.md file: a
fenced sh/bash block that runs `docker compose` with one of `exec`, `logs`,
`restart`, `stop`, `start`, `ps` or `run` and no `-f` acts on whichever
compose file is on the host's disk by default (see "Which ingress" in
deploy/README.md), which is wrong on a Traefik or direct-TLS host even when
it happens not to fail outright. `up`, `down`, `pull` and `config` are not
in that list: those commands are meant to touch the stack the `-f` files
name, and the surrounding prose is reviewed by hand instead, the same as a
compose file's own YAML would be. A block that names the marker
`doclint:allow-no-f` anywhere in it is exempted, for the one case where a
variable already carries `-f` (see docs/reference/github-actions-deploy.md).
"""
import copy
import json
import os
import re
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
FILES = {
    # file: the network its ingress reaches the server on
    'deploy/docker-compose.yml': 'tunnel',
    'deploy/docker-compose.traefik.yml': 'traefik',
}
# Every compose file, for the checks every deployment's procedures rely on.
ALL_FILES = list(FILES) + ['deploy/docker-compose.direct.yml']
SERVER, WORKER, BACKEND = 'recall-server', 'recall-worker', 'backend'
SQLITE_WEB = 'sqlite-web'
# The names deploy/README.md's restore stops and starts the containers by.
CONTAINER_NAMES = {SERVER: 'recall-server', SQLITE_WEB: 'recall-sqlite-web'}
# Asserted only in whichever files define the worker at all: direct TLS has
# no worker service, so it has nothing to name here.
WORKER_CONTAINER_NAME = 'recall-worker'
GRACE_SECONDS = 60
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


def seconds(duration):
    """A compose duration ("60s", "1m", "1m30s", or "1m0s" as `docker compose
    config` writes it back) in seconds; None if it is not one."""
    if isinstance(duration, (int, float)):
        return float(duration)
    parts = re.findall(r'(\d+(?:\.\d+)?)(h|ms|us|ns|m|s)', str(duration or ''))
    if not parts or ''.join(n + u for n, u in parts) != str(duration):
        return None
    scale = {'h': 3600, 'm': 60, 's': 1, 'ms': 1e-3, 'us': 1e-6, 'ns': 1e-9}
    return sum(float(n) * scale[u] for n, u in parts)


def server_problems(doc):
    """What every compose file must have for deploy/README.md's procedures."""
    found = []
    services = doc.get('services') or {}
    for service, name in CONTAINER_NAMES.items():
        got = (services.get(service) or {}).get('container_name')
        if got != name:
            found.append(f'{service} is container_name: {name}, which the restore stops '
                         f'and starts it by, not {got!r}')
    grace = seconds((services.get(SERVER) or {}).get('stop_grace_period'))
    if grace is None or grace < GRACE_SECONDS:
        found.append(f'{SERVER} has stop_grace_period of at least {GRACE_SECONDS}s, so a '
                     'stop reaches the checkpoint that empties the WAL')
    if WORKER in services:
        got = services[WORKER].get('container_name')
        if got != WORKER_CONTAINER_NAME:
            found.append(f'{WORKER} is container_name: {WORKER_CONTAINER_NAME}, which '
                         f'deploy/README.md logs, restarts and stops it by, not {got!r}')
    return found


def server_mutations(doc):
    """Copies of `doc`, each with one thing server_problems must refuse."""
    def changed(label, service, change):
        d = copy.deepcopy(doc)
        change(d['services'][service])
        return label, d

    muts = [
        changed('no stop_grace_period', SERVER, lambda s: s.pop('stop_grace_period', None)),
        changed('Docker\'s default grace', SERVER,
                lambda s: s.__setitem__('stop_grace_period', '10s')),
        changed('a renamed server container', SERVER,
                lambda s: s.__setitem__('container_name', 'recall-recall-server-1')),
        changed('no sqlite-web container name', SQLITE_WEB, lambda s: s.pop('container_name')),
    ]
    if WORKER in (doc.get('services') or {}):
        muts.append(changed('a renamed worker container', WORKER,
                             lambda s: s.__setitem__('container_name', 'recall-recall-worker-1')))
    return muts


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


# ---------------------------------------------------------------------------
# The doc lint: every fenced sh/bash block in deploy/README.md and
# docs/**/*.md, checked for a `docker compose` command that needs `-f` and
# does not carry it. See the module docstring for why this matters and what
# it deliberately leaves alone.

DOC_LINT_FILES = ['deploy/README.md']
DOC_LINT_SUBCOMMANDS = {'exec', 'logs', 'restart', 'stop', 'start', 'ps', 'run'}
DOC_LINT_MARKER = 'doclint:allow-no-f'
FENCE = re.compile(r'```(?:sh|bash)\n(.*?)```', re.S)


def doc_lint_files():
    """deploy/README.md, then every docs/**/*.md, relative to ROOT."""
    files = list(DOC_LINT_FILES)
    for dirpath, dirnames, filenames in sorted(os.walk(os.path.join(ROOT, 'docs'))):
        dirnames.sort()
        for name in sorted(filenames):
            if name.endswith('.md'):
                files.append(os.path.relpath(os.path.join(dirpath, name), ROOT))
    return files


def doc_lint_logical_lines(block):
    """One shell logical line per yield, backslash continuations joined."""
    buf = []
    for line in block.split('\n'):
        if line.endswith('\\'):
            buf.append(line[:-1])
            continue
        buf.append(line)
        yield ' '.join(buf)
        buf = []
    if buf:
        yield ' '.join(buf)


def doc_lint_problems(text):
    """Every `docker compose <op>` in `text` with no `-f`, `op` one of
    DOC_LINT_SUBCOMMANDS. `up`, `down`, `pull` and `config` are not checked:
    those are meant to act on the stack the `-f` files name, and reviewed by
    hand instead, as the module docstring says."""
    found = []
    for block in FENCE.findall(text):
        if DOC_LINT_MARKER in block:
            continue
        for line in doc_lint_logical_lines(block):
            for part in re.split(r'&&|;|\|', line):
                part = re.sub(r'(?:^|\s)#.*$', '', part)
                m = re.search(r'\bdocker\s+compose\b(.*)$', part)
                if not m:
                    continue
                tokens = m.group(1).split()
                hit = [t for t in tokens if t in DOC_LINT_SUBCOMMANDS]
                if not hit or '-f' in tokens or '--file' in tokens:
                    continue
                found.append(f"'docker compose {' '.join(tokens)}' has no -f: "
                             f"{hit[0]} needs the fixed container name (docker "
                             f"{hit[0]}) instead, or -f if it truly must be compose")
    return found


def main():
    fails = 0
    for path in ALL_FILES:
        doc = load(path)
        found = server_problems(doc)
        for p in found:
            print(f'  FAIL {path}: {p}')
        if not found:
            print(f'  ok   {path}: container names and stop_grace_period')
        fails += len(found)
        for label, bad in server_mutations(doc):
            if server_problems(bad):
                print(f'  ok   {path} with {label} is refused')
            else:
                print(f'  FAIL {path} with {label} passes the checks')
                fails += 1
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
    doc_fails = 0
    doc_files = doc_lint_files()
    for path in doc_files:
        with open(os.path.join(ROOT, path), encoding='utf-8') as f:
            found = doc_lint_problems(f.read())
        for p in found:
            print(f'  FAIL {path}: {p}')
        doc_fails += len(found)
    if doc_fails == 0:
        print(f'  ok   {len(doc_files)} doc files: every fenced sh/bash block '
              f'passes -f on exec/logs/restart/stop/start/ps/run')
    fails += doc_fails

    # The lint itself, proven against two small blocks it never reads from a
    # real file: one that must pass and one, missing only `-f`, that must not.
    compliant = '```sh\ndocker compose -f docker-compose.traefik.yml logs recall-server\n```\n'
    missing_f = '```sh\ndocker compose logs recall-server\n```\n'
    if doc_lint_problems(compliant):
        print('  FAIL the lint refuses a block that already passes -f')
        fails += 1
    else:
        print('  ok   the lint passes a block that already passes -f')
    if not doc_lint_problems(missing_f):
        print('  FAIL the lint passes a block missing -f')
        fails += 1
    else:
        print('  ok   the lint refuses a block missing -f')
    marked = missing_f.replace('```sh\n', f'```sh\n# {DOC_LINT_MARKER}\n', 1)
    if doc_lint_problems(marked):
        print(f'  FAIL the {DOC_LINT_MARKER} marker does not suppress the lint')
        fails += 1
    else:
        print(f'  ok   the {DOC_LINT_MARKER} marker suppresses the lint')

    print('all checks passed' if fails == 0 else f'{fails} FAILED')
    return 1 if fails else 0


if __name__ == '__main__':
    sys.exit(main())
