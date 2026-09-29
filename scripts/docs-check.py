"""The documentation still describes the code: what can be checked mechanically.

Run from anywhere:

    python3 scripts/docs-check.py                         # variables, index, changelog
    python3 scripts/docs-check.py --cli target/debug/recall  # commands and flags

This exists because the documentation drifted in ways nobody announced. In
September 2026 `recall-server`'s own configuration table named
`RECALL_HOST`, which nothing reads, and `RECALL_BACKUP_INTERVAL_MS`, which
had become `RECALL_BACKUP_INTERVAL_HOURS`; `docs/README.md` listed three
designs as "Not built" after two of them had shipped; and six flags of the
CLI were in no document at all. Each was true once. A check that runs on
every pull request finds the next one in the pull request that causes it,
not at release time, when the fix competes with the release.

What it checks, each in both directions where there are two:

  - **Variables.** Every `RECALL_*` variable the code reads is named in a
    document, and every one a document names is read by something. "Read"
    means a string literal in Rust outside a comment, or a non-comment line
    of the shipped shell, PowerShell and JavaScript and of the Dockerfile.
    "Named" means the reference docs, the top-level documents,
    `deploy/README.md`, the compose files (which set variables, so a
    variable they set that nothing reads is drift too), and comments in the
    code and scripts. `RECALL_FOO_*` in a document names a whole family.
  - **The index.** Every Markdown file under `docs/` is linked from
    `docs/README.md`, and every design in `docs/design/` opens with a
    `Status:` line, so the index and the file can be compared by a reader.
  - **The changelog.** `CHANGELOG.md` has a section for the version in
    `Cargo.toml`, so a version bump without its notes fails before the tag.
  - **The CLI** (`--cli BIN`, which needs a built binary): every command
    and subcommand `recall help` lists appears as `recall <command>` in the
    documents, and every long flag appears somewhere in them.

Two deliberate limits:

  - **It matches names, not meaning.** A flag is documented if its name is
    written down anywhere in the documents, not necessarily beside its
    command, and a variable whose documented default is wrong passes. The
    release checklist in `docs/reference/releasing.md` covers what a regex
    cannot: reading what changed and the documents that describe it.
  - **History is not checked.** `docs/history/`, `CHANGELOG.md` and
    `ROADMAP.md` record what was true then, and name removed variables on
    purpose.
"""
import argparse
import os
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
os.chdir(ROOT)

VAR = re.compile(r'RECALL_[A-Z0-9_]*[A-Z0-9](_\*)?')
RUST_LITERAL = re.compile(r'"(RECALL_[A-Z0-9_]*[A-Z0-9])"')

# Documents a reader is sent to for how things work today.
REFERENCE = [
    'README.md', 'ARCHITECTURE.md', 'CONTRIBUTING.md', 'deploy/README.md',
    *sorted(str(p) for p in Path('docs/reference').glob('*.md')),
]
# Shipped scripts and build files: a non-comment line reads, a comment names.
SCRIPTS = [
    'install.sh', 'install.ps1', 'deploy/Dockerfile',
    *sorted(str(p) for p in Path('deploy').glob('*.sh')),
    *sorted(str(p) for p in Path('npm').rglob('*.js') if 'node_modules' not in p.parts),
    *sorted(str(p) for p in Path('.claude/hooks').glob('*.sh')),
]
COMPOSE = sorted(str(p) for p in Path('deploy').glob('docker-compose*.yml'))

problems = []

# Variables a document names on purpose although nothing reads them, each
# with why. Keep this short: an entry is a promise that the mention is an
# example or a warning, never a setting someone might go and set.
NAMED_NOT_READ = {
    'RECALL_HOST': 'the worked example of a claim that names a variable '
                   'nothing reads, in recall review and install.md',
}


def note(where, name, found):
    found.setdefault(name, where)


def variables():
    read, named, families = {}, {}, set()

    def names(line, where):
        for m in VAR.finditer(line):
            if m.group(1):
                families.add(m.group(0)[:-1])
            else:
                note(where, m.group(0), named)

    for path in sorted(Path('crates').rglob('*.rs')):
        if 'target' in path.parts:
            continue
        for i, line in enumerate(path.read_text(encoding='utf-8').splitlines(), 1):
            where = f'{path}:{i}'
            if line.lstrip().startswith('//'):
                names(line, where)
            else:
                for v in RUST_LITERAL.findall(line):
                    note(where, v, read)
    for path in SCRIPTS:
        comment = '#'
        for i, line in enumerate(Path(path).read_text(encoding='utf-8').splitlines(), 1):
            where = f'{path}:{i}'
            stripped = line.lstrip()
            if stripped.startswith(comment) or stripped.startswith('//'):
                names(line, where)
            else:
                for m in VAR.finditer(line):
                    if not m.group(1):
                        note(where, m.group(0), read)
    for path in REFERENCE + COMPOSE:
        for i, line in enumerate(Path(path).read_text(encoding='utf-8').splitlines(), 1):
            names(line, f'{path}:{i}')

    def in_family(v):
        return any(v.startswith(f) for f in families)

    for v in sorted(set(read) - set(named)):
        if not in_family(v):
            problems.append(f'{v} is read ({read[v]}) but no document names it')
    for v in sorted(set(named) - set(read) - set(NAMED_NOT_READ)):
        problems.append(f'{v} is named ({named[v]}) but nothing reads it')
    for v in sorted(set(NAMED_NOT_READ) & set(read)):
        problems.append(f'{v} is read now; drop it from NAMED_NOT_READ')
    return len(read)


def index():
    text = Path('docs/README.md').read_text(encoding='utf-8')
    linked = {os.path.normpath(os.path.join('docs', t.split('#')[0]))
              for t in re.findall(r'\]\(([^)]+)\)', text)}
    count = 0
    for path in sorted(Path('docs').rglob('*.md')):
        if str(path) == 'docs/README.md':
            continue
        count += 1
        if str(path) not in linked:
            problems.append(f'{path} is not linked from docs/README.md')
    for path in sorted(Path('docs/design').glob('*.md')):
        head = path.read_text(encoding='utf-8').splitlines()[:6]
        if not any(line.startswith('Status:') for line in head):
            problems.append(f'{path} does not open with a Status: line')
    return count


def changelog():
    cargo = Path('Cargo.toml').read_text(encoding='utf-8')
    version = re.search(r'^version = "([^"]+)"', cargo, re.M).group(1)
    notes = Path('CHANGELOG.md').read_text(encoding='utf-8')
    if not re.search(rf'^## {re.escape(version)} — \d{{4}}-\d{{2}}-\d{{2}}', notes, re.M):
        problems.append(f'CHANGELOG.md has no "## {version} — YYYY-MM-DD" section '
                        'for the version in Cargo.toml')
    return version


def cli(binary):
    env = dict(os.environ, NO_COLOR='1', COLUMNS='200')
    for var in ('RECALL_URL', 'RECALL_TOKEN', 'RECALL_PROJECT_KEY', 'RECALL_AUTHKEY'):
        env.pop(var, None)
    corpus = ''.join(Path(p).read_text(encoding='utf-8') for p in REFERENCE)
    seen = []

    def walk(path):
        out = subprocess.run([binary, *path, '--help'], capture_output=True,
                             text=True, env=env, check=True).stdout
        subs, flags, section = [], [], None
        for line in out.splitlines():
            if line and not line.startswith(' '):
                section = line.rstrip(':')
            elif section == 'Options':
                flags += re.findall(r'^\s+(?:-\w, )?(--[a-z][a-z0-9-]*)', line)
            elif section not in (None, 'Usage', 'Arguments'):
                m = re.match(r'^  ([a-z][a-z0-9-]*)\s{2,}', line)
                if m and m.group(1) != 'help':
                    subs.append(m.group(1))
        command = ' '.join(['recall', *path])
        seen.append(command)
        if path and f'{command}' not in corpus:
            problems.append(f'`{command}` is in `recall help` but in no document')
        for flag in flags:
            if flag not in ('--help', '--version') and flag not in corpus:
                problems.append(f'`{command} {flag}` is in its --help but in no document')
        for sub in subs:
            walk([*path, sub])

    walk([])
    return len(seen)


parser = argparse.ArgumentParser(description=__doc__.split('\n')[0])
parser.add_argument('--cli', metavar='BIN', help='check a built recall binary\'s commands and flags')
args = parser.parse_args()

if args.cli:
    print(f'checked {cli(args.cli)} commands of {args.cli} against the documents')
else:
    n_vars = variables()
    n_docs = index()
    version = changelog()
    print(f'checked {n_vars} variables the code reads, {n_docs} indexed documents, '
          f'and the changelog for {version}')

if problems:
    print(f'OUT OF DATE ({len(problems)}):')
    for p in problems:
        print(f'  {p}')
    sys.exit(1)
print('the documents match')
