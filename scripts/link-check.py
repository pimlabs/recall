"""Every relative Markdown link in the tree resolves to a file that exists.

Run from anywhere:  python3 scripts/link-check.py

This exists because the doc set has been reorganised twice — `docs/` split
into `reference/` and `history/`, `tests/` renamed to `fixtures/` — and a
link that quietly starts pointing at nothing is the thing neither of those
moves would have announced. It found nothing when it was written. A check
that only earns its place *after* the next reorganisation is a check that
will not be there when it is needed.

Three deliberate limits, so nobody expects more than it gives:

  - **Fenced blocks are ignored.** Several documents quote what Claude writes
    into a memory file, and those samples contain Markdown links to files
    that live in someone's memory directory, not in this repository.
    `docs/history/memory-loading-findings.md` is the reason this rule exists:
    without it, that file reads as broken when it is correct.
  - **Anchors are not verified.** `install.md#check-its-working` is checked as
    `install.md`. Knowing whether a heading still exists would mean parsing
    and slugifying every heading the way GitHub does, and getting that subtly
    wrong is worse than not claiming it.
  - **External URLs are not fetched.** A dead link to somewhere else should
    not make a build red: it fails for reasons outside this repository, on
    someone else's schedule, and it would put a network call in CI.
"""
import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
os.chdir(ROOT)

# Markdown's `[text](target)`. Reference-style links and bare autolinks are
# not used anywhere in this tree; if they ever are, this misses them silently,
# which is the honest failure for a regex reading a real grammar.
LINK = re.compile(r'\[[^\]]*\]\(([^)]+)\)')
FENCED = re.compile(r'```.*?```', re.S)
ELSEWHERE = ('http://', 'https://', 'mailto:', '#')
SKIP_DIRS = {'.git', 'target', 'node_modules'}

broken = []
checked = 0
files = 0

for dirpath, dirnames, filenames in os.walk(ROOT):
    dirnames[:] = [d for d in dirnames if d not in SKIP_DIRS]
    for name in sorted(filenames):
        if not name.endswith('.md'):
            continue
        files += 1
        path = os.path.join(dirpath, name)
        with open(path, encoding='utf-8') as fh:
            text = FENCED.sub('', fh.read())

        for target in LINK.findall(text):
            # `[text](path "title")` — the title is not part of the path.
            target = target.split(' ')[0].strip()
            if not target or target.startswith(ELSEWHERE):
                continue
            # A same-file anchor has already been skipped above; this drops
            # the anchor from a cross-file link.
            file_part = target.split('#', 1)[0]
            if not file_part:
                continue
            checked += 1
            if not os.path.exists(os.path.normpath(os.path.join(dirpath, file_part))):
                broken.append(f'{os.path.relpath(path, ROOT)} -> {target}')

print(f'checked {checked} relative links across {files} markdown files')
if broken:
    print(f'BROKEN ({len(broken)}):')
    for entry in broken:
        print(f'  {entry}')
    sys.exit(1)
print('all resolve')
