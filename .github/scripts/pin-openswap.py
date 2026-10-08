#!/usr/bin/env python3
"""Pin and verify the exact upstream commit before running compatibility gates."""

import json
import os
from pathlib import Path
import re
import subprocess
import sys


UPSTREAM = 'https://github.com/citadel-foss/openswap'
DEPENDENCY = re.compile(r'^(openswap\s*=\s*\{[^\n]*\})', re.MULTILINE)
REVISION = re.compile(r'(\brev\s*=\s*")([0-9a-f]{40})(")')


def revision(manifest):
    dependency = DEPENDENCY.search(manifest)
    if not dependency or f'git = "{UPSTREAM}"' not in dependency[1]:
        raise ValueError('Expected one inline openswap git dependency from citadel-foss/openswap')
    matches = list(REVISION.finditer(dependency[1]))
    if len(matches) != 1:
        raise ValueError('Expected one full OpenSwap revision in Cargo.toml')
    return matches[0][2]


def pin_manifest(manifest, sha):
    if not re.fullmatch(r'[0-9a-f]{40}', sha):
        raise ValueError('OPENSWAP_SHA must be a full lowercase Git commit SHA')
    revision(manifest)
    return DEPENDENCY.sub(lambda match: REVISION.sub(
        lambda rev: rev[1] + sha + rev[3], match[1]), manifest, count=1)


def verify_metadata(metadata, sha):
    packages = [package for package in metadata['packages'] if package['name'] == 'openswap']
    expected = f'git+{UPSTREAM}?rev={sha}#{sha}'
    if len(packages) != 1 or packages[0].get('source') != expected:
        raise ValueError(f'Cargo did not resolve the selected OpenSwap commit {sha}')
    return packages[0]['source']


def pin(root, sha):
    manifest = root / 'Cargo.toml'
    manifest.write_text(pin_manifest(manifest.read_text(), sha))
    subprocess.run(['cargo', 'update', '-p', 'openswap', '--precise', sha], cwd=root, check=True)
    metadata = subprocess.check_output(
        ['cargo', 'metadata', '--locked', '--format-version', '1'], cwd=root, text=True)
    print('Resolved OpenSwap: ' + verify_metadata(json.loads(metadata), sha))


if __name__ == '__main__':
    if sys.argv[1:] == ['revision']:
        print(revision(sys.stdin.read()))
    elif not sys.argv[1:]:
        pin(Path.cwd(), os.environ['OPENSWAP_SHA'])
    else:
        raise SystemExit('Usage: pin-openswap.py [revision]')
