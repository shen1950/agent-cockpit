"""Offline publish gate. Reports locations/categories, never matched values.

Optional private deny terms live in ignored .agent-sync/private-terms.json.
No source scanner can certify the absence of all personal information.
"""
import argparse
import json
import re
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
EMAIL = re.compile(r'[\w.+*-]+@[\w.-]+\.[A-Za-z]{2,}')
USER_PATH = re.compile(r'(?i)[A-Z]:[\\/]+Users[\\/]+([^\\/\s"\'<>]+)')
# 开发者机器的安装根（不是产品默认位置）。公开源码里出现就说明本机路径没清干净。
DEV_ROOT = re.compile(r'(?i)\b[DE]:[\\/]+(?:Artificial_Intelligence|LenovoSoftstore|npm-global)\b')
SECRET = re.compile(r'\b(?:sk-[A-Za-z0-9_-]{20,}|gh[pousr]_[A-Za-z0-9]{25,}|AKIA[A-Z0-9]{16}|eyJ[A-Za-z0-9_-]{25,}\.[A-Za-z0-9_-]{25,}\.[A-Za-z0-9_-]{20,})')
# Exact synthetic fixtures used by encoding tests (not a file-wide exemption).
TEST_EMAILS = {'src-tauri/src/antigravity.rs': {'x@y.com', 'someone@gmail.com'}}
SYNTHETIC_KEYS = {
    'sk-aaaaaaaaaaaaaaaa1111', 'sk-bbbbbbbbbbbbbbbb2222', 'sk-cccccccccccccccc3333',
    'sk-tiez-0123456789abcdefghij', 'sk-pot-abcdefghijklmnopqrstuvwx',
    'sk-wb-aaaabbbbccccddddeeee', 'sk-lc-aaaaaaaaaaaaaaa1111',
    'sk-lc-bbbbbbbbbbbbbbb2222', 'sk-lc-cccccccccccccc3333',
    'sk-proj-0000000000000000abcd', 'sk-qcn-5555666677778888',
    'sk-ali-9999000011112222', 'sk-pro-3333444455556666',
}

def source_files(root=ROOT):
    if (root / '.git').exists():
        names = subprocess.check_output(['git', 'ls-files', '-z', '--cached', '--others', '--exclude-standard'], cwd=root).decode().split('\0')
        return sorted({root / n for n in names if n and (root / n).is_file()})
    return sorted(p for p in root.rglob('*') if p.is_file())

def private_terms(root=ROOT):
    path = root / '.agent-sync/private-terms.json'
    return json.loads(path.read_text(encoding='utf-8')) if path.exists() else []

def scan_text(text, name, terms=()):
    findings = []
    for line_number, line in enumerate(text.splitlines(), 1):
        categories = set()
        if any(re.search(r'(?<![A-Za-z])' + re.escape(term) + r'(?![A-Za-z])', line, re.I) for term in terms):
            categories.add('private-identity')
        for match in USER_PATH.finditer(line):
            if match[1].lower() not in {'example', 'test', 'x', 'me', 'public', 'default'}:
                categories.add('personal-home-path')
        if DEV_ROOT.search(line):
            categories.add('developer-install-root')
        for match in EMAIL.finditer(line):
            address = match[0]
            domain = address.split('@')[-1].lower()
            if not (domain.endswith(('.example', '.test', '.invalid')) or domain in {'example.com', 'example.org', 'example.net', '-example.com'} or address in TEST_EMAILS.get(name, set())):
                categories.add('non-example-email')
        for match in SECRET.finditer(line):
            if match[0] not in SYNTHETIC_KEYS:
                categories.add('credential-like-literal')
        if re.search(r'-----BEGIN (?:RSA |EC |OPENSSH )?PRIVATE KEY-----', line):
            categories.add('private-key')
        findings.extend((name, line_number, category) for category in sorted(categories))
    return findings

def scan_binary(path, terms):
    data = path.read_bytes()
    return [(path.name, 0, 'embedded-private-identity')] if any(
        term.encode(encoding) in data for term in terms for encoding in ('utf-8', 'utf-16-le')
    ) else []

def scan(root=ROOT):
    findings = []
    terms = private_terms(root)
    # Tauri embeds frontendDist even when a file is ignored by Git.
    files = set(source_files(root))
    files.update(p for p in (root / 'ui').rglob('*') if p.is_file())
    for path in sorted(files):
        name = path.relative_to(root).as_posix()
        if name.startswith('ui/') and ('.bak' in path.name or path.name.endswith(('-bak', '.log', '.map'))):
            findings.append((name, 0, 'unexpected-bundled-artifact'))
        if name == 'tools/privacy_check.py':
            continue  # rule definitions are not product data
        try:
            text = path.read_text(encoding='utf-8-sig')
        except (UnicodeError, OSError):
            findings.extend(scan_binary(path, terms))
            continue
        findings.extend(scan_text(text, name, terms))
    return findings

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--binary', type=Path)
    args = parser.parse_args()
    findings = scan_binary(args.binary, private_terms()) if args.binary else scan()
    for name, line, category in findings:
        print(f'{name}:{line}: {category}')
    print(f'Privacy gate: {len(findings)} findings; values withheld.')
    return bool(findings)

if __name__ == '__main__':
    raise SystemExit(main())
