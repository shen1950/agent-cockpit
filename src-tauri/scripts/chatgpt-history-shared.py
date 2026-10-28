"""One local history store, independent credentials and browser preferences.

The already running native CODEX_HOME owns the physical rollout directories and
SQLite WAL databases. Private homes use sqlite_home plus narrowly validated
directory junctions for sessions/archived_sessions. No auth directory is linked.
"""
from contextlib import closing
import base64
import gzip
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import shutil
import sqlite3
import subprocess
import sys
import time
import tomllib
import types
import uuid

DIRECTORIES = ('sessions', 'archived_sessions')
LOCK_DIRECTORY = 'thread-writer-locks'
MARKER = '.agentcockpit-shared-history.json'


def support(name, text=None):
    module = types.ModuleType(name)
    if text is None:
        filename = 'chatgpt-history-sync.py' if name == 'replica' else 'chatgpt-history.py'
        text = Path(__file__).with_name(filename).read_text(encoding='utf-8')
    exec(compile(text, '<' + name + '>', 'exec'), module.__dict__)
    return module


def config_bytes(path, canonical):
    original = path.read_bytes() if path.is_file() else b''
    text = original.decode('utf-8-sig')
    config = tomllib.loads(text)
    if config.get('sqlite_home') == str(canonical):
        return original, original
    lines, in_root = [], True
    for line in text.splitlines(keepends=True):
        if line.lstrip().startswith('['):
            in_root = False
        if in_root and re.match(r"\s*(?:sqlite_home|\"sqlite_home\"|'sqlite_home')\s*=", line):
            continue
        lines.append(line)
    updated = 'sqlite_home = ' + json.dumps(str(canonical), ensure_ascii=False) + '\n' + ''.join(lines)
    expected = dict(config, sqlite_home=str(canonical))
    if tomllib.loads(updated) != expected:
        raise ValueError('Account configuration could not be preserved')
    return original, updated.encode('utf-8')


def write_bytes(path, data):
    temp = path.with_name(path.name + '.' + uuid.uuid4().hex + '.tmp')
    try:
        temp.write_bytes(data)
        os.replace(temp, path)
    finally:
        temp.unlink(missing_ok=True)


def link_directory(path, target):
    if os.name == 'nt':
        # Encoded PowerShell keeps metacharacters in account names literal.
        literal = lambda p: "'" + str(p).replace("'", "''") + "'"
        command = "$ErrorActionPreference='Stop'; New-Item -ItemType Junction -Path " + literal(path) + ' -Target ' + literal(target) + ' | Out-Null'
        encoded = base64.b64encode(command.encode('utf-16-le')).decode('ascii')
        subprocess.run(['powershell.exe', '-NoProfile', '-NonInteractive', '-WindowStyle', 'Hidden', '-EncodedCommand', encoded],
                       check=True, capture_output=True, creationflags=0x08000000, timeout=20)
    else:
        path.symlink_to(target, target_is_directory=True)


def exact_link(path, target):
    try:
        return path.exists() and path.resolve() == target.resolve() and path.absolute() != target.absolute()
    except OSError:
        return False


def private_files(directory):
    for parent, dirs, files in os.walk(directory, followlinks=False):
        for name in dirs:
            yield Path(parent) / name, True
        for name in files:
            yield Path(parent) / name, False


class SharedHistory:
    def __init__(self, canonical, state_dir, replica, history=None):
        self.replica, self.history = replica, history
        self.canonical = replica.plain(Path(canonical))
        self.state_dir = replica.plain(Path(state_dir))
        self.state_dir.mkdir(parents=True, exist_ok=True)
        self.rep = replica.Replicator(state_dir)
        self.report = {'ok': True, 'version': 2, 'storage': 'shared', 'canonical': str(self.canonical),
                       'updated_at': time.time(), 'homes': 1, 'migrated': [], 'pending': [],
                       'conflicts': [], 'released_bytes': 0}
        for name in DIRECTORIES:
            replica.plain(self.canonical / name).mkdir(parents=True, exist_ok=True)
        replica.plain(self.canonical / LOCK_DIRECTORY).mkdir(parents=True, exist_ok=True)

    def marker(self, home):
        path = home / MARKER
        if not path.exists():
            return None
        self.replica.plain(path)
        value = json.loads(path.read_text(encoding='utf-8'))
        if value.get('canonical') != str(self.canonical):
            raise ValueError('Account is linked to a different history store')
        return value

    def verify_links(self, home):
        for name in DIRECTORIES:
            if not exact_link(home / name, self.canonical / name):
                raise ValueError('Shared history link is missing or points elsewhere')
        if tomllib.loads((home / 'config.toml').read_text(encoding='utf-8-sig')).get('sqlite_home') != str(self.canonical):
            raise ValueError('Account SQLite state is not shared')

    def ensure_lock_directory(self, home):
        source, target = home / LOCK_DIRECTORY, self.canonical / LOCK_DIRECTORY
        if exact_link(source, target):
            return
        self.replica.plain(source)
        old = home / ('.history-locks-before-shared-' + uuid.uuid4().hex)
        existed = source.exists()
        if existed:
            source.rename(old)
        try:
            link_directory(source, target)
            if not exact_link(source, target):
                raise ValueError('Shared writer lock link verification failed')
        except Exception:
            if exact_link(source, target):
                os.rmdir(source) if os.name == 'nt' else source.unlink()
            if existed and not source.exists():
                old.rename(source)
            raise

    def normalize_paths(self, homes):
        db = self.canonical / 'state_5.sqlite'
        if not db.exists():
            return
        with closing(self.replica.connect(db, True)) as con, con:
            for row in con.execute('SELECT id,rollout_path FROM threads').fetchall():
                raw = row['rollout_path']
                path = Path(raw[4:] if raw.startswith('\\\\?\\') else raw)
                for home in homes:
                    try:
                        relative = path.relative_to(home)
                    except ValueError:
                        continue
                    if relative.parts and relative.parts[0] in DIRECTORIES:
                        destination = self.canonical / relative
                        if destination.is_file() and str(destination) != raw:
                            con.execute('UPDATE threads SET rollout_path=? WHERE id=? AND rollout_path=?', (str(destination), row['id'], raw))
                        break

    def merge_projects(self, home):
        src, dst = home / 'state_5.sqlite', self.canonical / 'state_5.sqlite'
        if not src.exists() or not dst.exists():
            return
        with closing(self.replica.connect(src)) as a, closing(self.replica.connect(dst, True)) as b, b:
            for table in ('projects', 'project_roots', 'thread_sections'):
                for row in self.replica.rows(a, table):
                    self.rep.upsert(b, table, row, False)

    def merge_files(self, home):
        # Include orphaned and archived rollouts as well as indexed threads.
        for name in DIRECTORIES:
            source = home / name
            if not source.exists():
                continue
            self.replica.plain(source)
            for path, directory in private_files(source):
                self.replica.plain(path)
                if directory:
                    continue
                destination = self.canonical / name / path.relative_to(source)
                self.replica.plain(destination)
                if not destination.exists():
                    destination.parent.mkdir(parents=True, exist_ok=True)
                    shutil.copyfile(path, destination)
                elif path.read_bytes() != destination.read_bytes():
                    if path.suffix != '.jsonl':
                        raise ValueError('An archive has different content in two homes: ' + path.name)
                    old, new = self.replica.snapshot(path), self.replica.snapshot(destination)
                    if self.replica.replace_offset(old, new) is not None:
                        continue
                    if self.replica.replace_offset(new, old) is None:
                        raise ValueError('Conflicting conversation continuations: ' + path.name)
                    self.rep.publish(self.canonical, destination, old)

    def initialize_preferences(self, home):
        # Per-window settings remain local and existing preferences win.
        target = home / '.codex-global-state.json'
        original = self.canonical / '.codex-global-state.json'
        if not target.exists() and original.is_file() and self.history:
            _, relocate = self.history.make_relocator(self.canonical, home)
            self.replica.atomic_json(target, self.history.history_state(original, relocate))

    def check_database_union(self, home):
        db = home / 'state_5.sqlite'
        shared = self.canonical / 'state_5.sqlite'
        if not db.is_file():
            return
        with closing(self.replica.connect(db)) as a, closing(self.replica.connect(shared)) as b:
            ids = {r[0] for r in b.execute('SELECT id FROM threads')}
            private = {r[0] for r in a.execute('SELECT id FROM threads')}
            if not private <= ids:
                raise ValueError('Some private threads are not in the shared database')

    def retire_databases(self, home):
        self.check_database_union(home)
        tag = hashlib.sha256(str(home).encode()).hexdigest()[:16]
        backup_root = self.state_dir / 'before-shared' / tag
        for path in home.iterdir():
            if not re.fullmatch(r'(?:state_\d+|thread_history_\d+)\.sqlite', path.name):
                continue
            self.replica.plain(path)
            backup_root.mkdir(parents=True, exist_ok=True)
            backup = backup_root / (path.name + '.gz')
            if not backup.exists():
                snapshot = backup_root / (path.name + '.snapshot-' + uuid.uuid4().hex)
                packed = backup.with_name(backup.name + '.' + uuid.uuid4().hex + '.tmp')
                try:
                    with closing(self.replica.connect(path)) as src, closing(sqlite3.connect(snapshot)) as dst:
                        src.backup(dst)
                    with snapshot.open('rb') as src, gzip.open(packed, 'wb', compresslevel=6) as dst:
                        shutil.copyfileobj(src, dst)
                    with gzip.open(packed, 'rb') as check, snapshot.open('rb') as original:
                        if hashlib.file_digest(check, 'sha256').digest() != hashlib.file_digest(original, 'sha256').digest():
                            raise ValueError('History rollback backup failed verification')
                    os.replace(packed, backup)
                finally:
                    snapshot.unlink(missing_ok=True)
                    packed.unlink(missing_ok=True)
            # The private client is stopped and sqlite_home has already changed.
            for suffix in ('-wal', '-shm', ''):
                item = Path(str(path) + suffix)
                self.replica.plain(item)
                if item.exists():
                    size = item.stat().st_size
                    item.unlink()
                    self.report['released_bytes'] += size

    def cleanup_stages(self, home, value):
        for name, stage_name in list(value.get('stages', {}).items()):
            stage = home / stage_name
            self.replica.plain(stage)
            # Validate the final absolute recursive-delete target first.
            if stage.parent.resolve() != home.resolve() or not stage.name.startswith('.history-shared-stage-'):
                raise ValueError('Invalid history migration cleanup path')
            if not stage.exists():
                continue
            for path, directory in private_files(stage):
                self.replica.plain(path)
                if directory:
                    continue
                shared = self.canonical / name / path.relative_to(stage)
                if not shared.is_file():
                    raise ValueError('A private file has not been preserved')
                a, b = path.read_bytes(), shared.read_bytes()
                if a != b and (path.suffix != '.jsonl' or self.replica.replace_offset(self.replica.snapshot(path), self.replica.snapshot(shared)) is None):
                    raise ValueError('A private continuation cannot be discarded')
            released = sum(p.stat().st_size for p, directory in private_files(stage) if not directory)
            shutil.rmtree(stage)
            self.report['released_bytes'] += released
        value['stages'] = {}
        self.replica.atomic_json(home / MARKER, value)

    def migrate(self, home):
        home = self.replica.plain(Path(home))
        if home == self.canonical:
            return
        home.mkdir(parents=True, exist_ok=True)
        previous = self.marker(home)
        if previous:
            self.verify_links(home)
            # Recovery after an interrupted migration is conservative: old
            # databases and staged rollouts remain available for manual audit.
            if any(home.glob('state_*.sqlite')) or any(home.glob('thread_history_*.sqlite')) or previous.get('stages'):
                raise ValueError('Interrupted migration needs history verification; previous files retained')
            self.ensure_lock_directory(home)
            self.retire_databases(home)
            self.cleanup_stages(home, previous)
            return
        original, updated = config_bytes(home / 'config.toml', self.canonical)
        # All legacy content is reconciled while both physical copies exist.
        if (home / 'state_5.sqlite').exists() and (self.canonical / 'state_5.sqlite').exists():
            merged = self.rep.run([self.canonical, home])
            if merged['conflicts']:
                raise ValueError('Conflicting history remains in this account')
            if merged['deferred']:
                raise ValueError('History is still being written or could not be merged; retry later')
            self.merge_projects(home)
            self.check_database_union(home)
        self.merge_files(home)
        self.initialize_preferences(home)
        stages, linked = {}, []
        try:
            for name in DIRECTORIES:
                source = home / name
                if source.exists():
                    self.replica.plain(source)
                    stage = home / ('.history-shared-stage-' + name + '-' + uuid.uuid4().hex)
                    source.rename(stage)
                    stages[name] = stage.name
                link_directory(source, self.canonical / name)
                linked.append(name)
                if not exact_link(source, self.canonical / name):
                    raise ValueError('Shared directory link verification failed')
            if original != updated:
                before = home / '.shared-history-config.previous.toml'
                if not before.exists():
                    before.write_bytes(original)
                write_bytes(home / 'config.toml', updated)
            self.verify_links(home)
            self.ensure_lock_directory(home)
            value = {'version': 1, 'canonical': str(self.canonical), 'migrated_at': time.time(), 'stages': stages}
            self.replica.atomic_json(home / MARKER, value)
        except Exception:
            for name in reversed(linked):
                link = home / name
                if exact_link(link, self.canonical / name):
                    if os.name == 'nt':
                        os.rmdir(link)
                    else:
                        link.unlink()
            for name, stage in stages.items():
                if not (home / name).exists():
                    (home / stage).rename(home / name)
            if original != updated:
                write_bytes(home / 'config.toml', original)
            raise
        self.normalize_paths([home, self.canonical])
        self.retire_databases(home)
        self.cleanup_stages(home, value)
        self.report['migrated'].append(str(home))

    def run(self, homes, busy=()):
        homes = list(dict.fromkeys(self.replica.plain(Path(h)) for h in homes))
        if self.canonical not in homes:
            homes.insert(0, self.canonical)
        if any(a != b and (a in b.parents or b in a.parents) for a in homes for b in homes):
            raise ValueError('History homes overlap')
        busy = {str(Path(h)) for h in busy}
        for home in homes:
            if home == self.canonical:
                continue
            if str(home) in busy:
                if self.marker(home):
                    try:
                        self.verify_links(home)
                        if not exact_link(home / LOCK_DIRECTORY, self.canonical / LOCK_DIRECTORY):
                            self.report['pending'].append({'home': str(home), 'reason': 'Close the account window to enable the shared writer lock'})
                    except (OSError, ValueError) as error:
                        self.report['pending'].append({'home': str(home), 'reason': str(error)})
                else:
                    self.report['pending'].append({'home': str(home), 'reason': 'Account window is running'})
                continue
            try:
                self.migrate(home)
            except (OSError, ValueError, sqlite3.Error, subprocess.SubprocessError) as error:
                self.report['pending'].append({'home': str(home), 'reason': str(error)})
        self.normalize_paths(homes)
        # Only rebuild the small per-window sidebar cache; never copy message
        # text, SQLite databases or session files after migration.
        db = self.canonical / 'state_5.sqlite'
        if db.is_file():
            with closing(self.replica.connect(db)) as con:
                threads = {r['id']: r for r in self.replica.rows(con, 'threads')}
            backup = self.rep.backup
            self.rep.backup = lambda *args, **kwargs: None
            try:
                for home in homes:
                    if home != self.canonical and not self.marker(home):
                        continue
                    self.rep.clean_internal_catalog(home, threads)
                    for row in threads.values():
                        self.rep.catalog(home, row)
            finally:
                self.rep.backup = backup
        self.report['homes'] = len(homes)
        self.report['updated_at'] = time.time()
        self.replica.atomic_json(self.state_dir / 'shared.json', {'version': 2, 'canonical': str(self.canonical), 'homes': [str(h) for h in homes]})
        self.replica.atomic_json(self.state_dir / 'status.json', self.report)
        return self.report


def synchronize(request):
    replica = support('replica', request.get('sync_script'))
    history = support('history', request.get('history_script')) if request.get('history_script') or '__file__' in globals() else None
    with replica.lock_file(Path(request['state_dir']) / 'sync.lock'):
        return SharedHistory(request['canonical'], request['state_dir'], replica, history).run(request['homes'], request.get('busy_homes', []))


if __name__ == '__main__':
    try:
        result = synchronize(json.load(sys.stdin))
        print(json.dumps(result, ensure_ascii=False))
    except Exception as error:
        print(json.dumps({'ok': False, 'message': str(error)}, ensure_ascii=False))
        sys.exit(1)
