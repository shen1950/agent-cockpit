"""Invoke through agsync build --cmd/--test-cmd, never bypass its lock."""
import os
from pathlib import Path
import subprocess
import sys

root = Path(__file__).resolve().parents[1]
env = os.environ.copy()
flags = env.get('CARGO_ENCODED_RUSTFLAGS', '').split('\x1f') if env.get('CARGO_ENCODED_RUSTFLAGS') else env.get('RUSTFLAGS', '').split()
for source, target in [(root, '/source/agent-cockpit'), (root.parent, '/source'), (Path.home(), '/home/build')]:
    for spelling in dict.fromkeys((str(source), source.as_posix())):
        flags.append(f'--remap-path-prefix={spelling}={target}')
env.pop('RUSTFLAGS', None)
env['CARGO_ENCODED_RUSTFLAGS'] = '\x1f'.join(flags)
env['CARGO_NET_OFFLINE'] = 'true'
arguments = sys.argv[1:]
if '--release' in arguments:
    arguments += ['--features', 'custom-protocol']
raise SystemExit(subprocess.call(['cargo', *arguments], cwd=root / 'src-tauri', env=env))
