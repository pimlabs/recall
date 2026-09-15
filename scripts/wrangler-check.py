"""wrangler.toml says what we think it says — the keys, and where they landed.

Run from the repository root:  python3 scripts/wrangler-check.py
"""
import os
import tomllib

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
os.chdir(ROOT)

d = tomllib.load(open('wrangler.toml', 'rb'))

fails = 0


def check(label, actual, expected):
    global fails
    ok = actual == expected
    if not ok:
        fails += 1
    print(f'  {"ok  " if ok else "FAIL"} {label}{"" if ok else f": {actual!r} != {expected!r}"}')


check('name', d.get('name'), 'recall-install')
check('main', d.get('main'), 'install-worker.js')
check('entrypoint exists on disk', os.path.exists(d.get('main', '')), True)
check('compatibility_date is set', bool(d.get('compatibility_date')), True)

# The one that bit: a top-level key written after [[routes]] becomes a route
# key instead, and the workers.dev subdomain quietly stays enabled.
check('workers_dev is top-level, not inside a route', d.get('workers_dev'), False)

routes = d.get('routes', [])
check('exactly one route', len(routes), 1)
if routes:
    r = routes[0]
    check('route covers the whole host', r.get('pattern'), 'recall.pimlabs.id/*')
    check('route names the zone', r.get('zone_name'), 'pimlabs.id')
    check('route carries no stray keys', sorted(r), ['pattern', 'zone_name'])

# Nothing that would put a credential at the edge.
for key in ('vars', 'secrets', 'kv_namespaces', 'd1_databases', 'r2_buckets'):
    check(f'no {key}', key in d, False)

print('all checks passed' if fails == 0 else f'{fails} FAILED')
raise SystemExit(1 if fails else 0)
