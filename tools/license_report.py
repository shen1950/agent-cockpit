"""Report third-party crate licenses from Cargo.lock and the local registry cache.

Resolved crate metadata is read from the cargo registry source cache
($CARGO_HOME/registry/src), so the report reflects what actually gets compiled.
Only package names, versions and SPDX expressions are printed - no local paths.
"""
import argparse
import os
import re
from collections import defaultdict
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
LOCK = ROOT / 'src-tauri' / 'Cargo.lock'

NAME = re.compile(r'^name = "(.*?)"', re.M)
VERSION = re.compile(r'^version = "(.*?)"', re.M)
SOURCE = re.compile(r'^source = "(.*?)"', re.M)
LICENSE = re.compile(r'^license\s*=\s*"(.*?)"', re.M)
LICENSE_FILE = re.compile(r'^\s*license-file\s*=', re.M)


def cargo_home():
    home = os.environ.get('CARGO_HOME')
    if home:
        return Path(home)
    return Path(os.path.expanduser('~')) / '.cargo'


def locked_packages(lock=LOCK):
    """Third-party packages only: entries without `source` are workspace members."""
    packages = []
    for block in lock.read_text(encoding='utf-8').split('[[package]]')[1:]:
        name, version, source = NAME.search(block), VERSION.search(block), SOURCE.search(block)
        if name and version and source:
            packages.append((name[1], version[1]))
    return sorted(packages)


def registry_sources():
    src = cargo_home() / 'registry' / 'src'
    return [path for path in src.iterdir() if path.is_dir()] if src.is_dir() else []


def read_license(name, version, hosts):
    for host in hosts:
        direct = host / f'{name}-{version}' / 'Cargo.toml'
        if direct.exists():
            found = LICENSE.search(direct.read_text(encoding='utf-8', errors='replace'))
            if found:
                return found[1]
            return 'LICENSE-FILE' if LICENSE_FILE.search(
                direct.read_text(encoding='utf-8', errors='replace')) else 'UNKNOWN'
    for host in hosts:  # cached under a slightly different version string
        matches = sorted(p for p in host.iterdir() if p.is_dir() and re.match(
            rf'^{re.escape(name)}-\d', p.name))
        if matches:
            text = (matches[-1] / 'Cargo.toml').read_text(encoding='utf-8', errors='replace')
            found = LICENSE.search(text)
            if found:
                return found[1]
            return 'LICENSE-FILE' if LICENSE_FILE.search(text) else 'UNKNOWN'
    return 'UNKNOWN'


def collect():
    hosts = registry_sources()
    grouped = defaultdict(list)
    for name, version in locked_packages():
        grouped[read_license(name, version, hosts)].append(f'{name} {version}')
    return grouped


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--format', choices=('text', 'md'), default='text')
    parser.add_argument('--out', type=Path)
    parser.add_argument('--top', type=int, default=0,
                        help='only print the N largest buckets (0 = all)')
    args = parser.parse_args()

    grouped = collect()
    total = sum(len(items) for items in grouped.values())
    buckets = sorted(grouped.items(), key=lambda item: (-len(item[1]), item[0]))
    if args.top:
        buckets = buckets[:args.top]

    lines = []
    for name, items in buckets:
        lines.append(f'## {name}  ({len(items)})')
        lines.append(', '.join(sorted(items)))
        lines.append('')
    lines.append(f'Total: {total} third-party packages in {len(grouped)} license buckets.')
    body = '\n'.join(lines)

    if args.out:
        args.out.write_text(body + '\n', encoding='utf-8')
        print(f'License report written: {args.out.name}')
    else:
        print(body)
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
