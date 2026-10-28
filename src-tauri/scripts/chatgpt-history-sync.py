"""Incremental, additive history replication between registered local Codex homes.

Never copy auth/config, browser profiles, jobs or whole live databases. SQLite
transactions preserve WAL writers. Existing rollouts can only grow by a verified
prefix; conflicting continuations are retained in their original homes.
"""
from contextlib import closing, contextmanager
import hashlib
import json
import os
from pathlib import Path
import sqlite3
import stat
import sys
import time
import uuid


def plain(path):
    path = Path(path)
    if not path.is_absolute() or '..' in path.parts:
        raise ValueError('History paths must be absolute')
    for part in (path, *path.parents):
        try:
            info = part.lstat()
        except FileNotFoundError:
            continue
        if stat.S_ISLNK(info.st_mode) or getattr(info, 'st_file_attributes', 0) & 0x400:
            raise ValueError('History links are not supported: ' + str(part))
    return path


def connect(path, write=False):
    plain(path)
    con = sqlite3.connect(Path(path).as_uri() + ('?mode=rw' if write else '?mode=ro'), uri=True, timeout=2)
    con.row_factory = sqlite3.Row
    if not write:
        con.execute('PRAGMA query_only=ON')
    return con


def qi(name):
    return '"' + name.replace('"', '""') + '"'


def columns(con, table):
    return [r['name'] for r in con.execute('PRAGMA table_info(' + qi(table) + ')')]


def rows(con, table, thread=None):
    cols = columns(con, table)
    if not cols:
        return []
    sql, args = 'SELECT * FROM ' + qi(table), ()
    if thread is not None:
        if 'thread_id' not in cols:
            return []
        sql += ' WHERE thread_id=?'
        args = (thread,)
    return [dict(r) for r in con.execute(sql, args)]


def atomic_json(path, value):
    plain(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    temp = path.with_name(path.name + '.' + uuid.uuid4().hex + '.tmp')
    try:
        temp.write_text(json.dumps(value, ensure_ascii=False), encoding='utf-8')
        os.replace(temp, path)
    finally:
        temp.unlink(missing_ok=True)


@contextmanager
def lock_file(path):
    plain(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open('a+b') as file:
        file.seek(0)
        if os.name == 'nt':
            import msvcrt
            msvcrt.locking(file.fileno(), msvcrt.LK_NBLCK, 1)
        else:
            import fcntl
            fcntl.flock(file, fcntl.LOCK_EX | fcntl.LOCK_NB)
        try:
            yield
        finally:
            file.seek(0)
            if os.name == 'nt':
                msvcrt.locking(file.fileno(), msvcrt.LK_UNLCK, 1)
            else:
                fcntl.flock(file, fcntl.LOCK_UN)


def snapshot(path):
    """Ignore a last partial JSONL record while the producer is writing it."""
    plain(path)
    before = path.stat()
    data = path.read_bytes()
    after = path.stat()
    if (before.st_size, before.st_mtime_ns) != (after.st_size, after.st_mtime_ns):
        raise BlockingIOError('Rollout changed during snapshot')
    if data and not data.endswith(b'\n'):
        data = data[:data.rfind(b'\n') + 1]
    if data:
        json.loads(data.splitlines()[0])
        json.loads(data.rstrip().rsplit(b'\n', 1)[-1])
    return data


def settings_only(line):
    record = json.loads(line)
    return record.get('type') == 'event_msg' and record.get('payload', {}).get('type') == 'thread_settings_applied'


def internal_thread(row):
    if row.get('thread_source') in ('subagent', 'guardian_review'):
        return True
    try:
        origin = json.loads(row.get('source', ''))
    except (ValueError, TypeError):
        return False
    return isinstance(origin, dict) and 'subagent' in origin


def replace_offset(current, incoming):
    if incoming.startswith(current):
        return len(current)
    old, new = current.splitlines(keepends=True), incoming.splitlines(keepends=True)
    common = 0
    for a, b in zip(old, new):
        if a != b:
            break
        common += 1
    # Opening an old copied chat appends window-local settings records. These
    # are not a competing conversation continuation. Preserve their original
    # bytes in the backup, then replace only that settings-only suffix.
    if common and all(settings_only(line) for line in old[common:]):
        return sum(map(len, old[:common]))
    return None


def rollout_path(root, row):
    raw = row.get('rollout_path', '')
    if raw.startswith('\\\\?\\'):
        raw = raw[4:]
    path = plain(Path(raw))
    # Old copied DB rows may still name their previous home. Never read auth,
    # config, or arbitrary files through a poisoned rollout_path.
    relative = path.relative_to(root)
    if relative.parts[0] not in ('sessions', 'archived_sessions') or path.suffix != '.jsonl':
        raise ValueError('Rollout is outside this home history')
    return path, relative


class Replicator:
    def __init__(self, state_dir):
        self.directory = plain(Path(state_dir))
        self.directory.mkdir(parents=True, exist_ok=True)
        self.cache_file = self.directory / 'fingerprints.json'
        try:
            self.cache = json.loads(self.cache_file.read_text(encoding='utf-8'))
        except FileNotFoundError:
            self.cache = {}
        self.report = {'ok': True, 'version': 1, 'updated_at': time.time(),
                       'homes': 0, 'threads': 0, 'rollouts': 0, 'catalog': 0,
                       'conflicts': [], 'deferred': [], 'missing': []}

    def backup(self, root, path, database=False):
        tag = hashlib.sha256(str(root).encode()).hexdigest()[:16]
        destination = plain(self.directory / 'backups-v1' / tag / path.relative_to(root))
        if destination.exists():
            return
        destination.parent.mkdir(parents=True, exist_ok=True)
        if database:
            with closing(connect(path)) as source, closing(sqlite3.connect(destination)) as target:
                source.backup(target)
        else:
            with destination.open('xb') as out:
                out.write(path.read_bytes())

    def fingerprint(self, path):
        info = path.stat()
        token = [info.st_size, info.st_mtime_ns]
        old = self.cache.get(str(path))
        if old and 'meaningful' in old and old['stat'] == token:
            return old
        data = snapshot(path)
        value = {'stat': token, 'size': len(data), 'hash': hashlib.sha256(data).hexdigest(),
                 'meaningful': sum(not settings_only(line) for line in data.splitlines())}
        self.cache[str(path)] = value
        return value

    def publish(self, root, destination, data):
        plain(destination)
        destination.parent.mkdir(parents=True, exist_ok=True)
        if not destination.exists():
            temp = destination.with_name('.sync-' + uuid.uuid4().hex)
            try:
                temp.write_bytes(data)
                # Exclusive publication: a concurrent client can create the
                # target between inspection and this call; never overwrite it.
                os.link(temp, destination)
            finally:
                temp.unlink(missing_ok=True)
            return True
        current = snapshot(destination)
        if current == data:
            return False
        offset = replace_offset(current, data)
        if offset is None:
            raise ValueError('Conflicting continuation')
        self.backup(root, destination)
        with destination.open('r+b') as file:
            if os.name == 'nt':
                import msvcrt
                msvcrt.locking(file.fileno(), msvcrt.LK_NBLCK, 0x7fffffff)
            else:
                import fcntl
                fcntl.flock(file, fcntl.LOCK_EX | fcntl.LOCK_NB)
            try:
                # Recheck under the OS lock, including a producer's partial tail.
                if file.read() != current:
                    raise BlockingIOError('Destination has a concurrent writer')
                file.seek(offset)
                file.write(data[offset:])
                file.truncate()
                file.flush()
                os.fsync(file.fileno())
            finally:
                file.seek(0)
                if os.name == 'nt':
                    msvcrt.locking(file.fileno(), msvcrt.LK_UNLCK, 0x7fffffff)
                else:
                    fcntl.flock(file, fcntl.LOCK_UN)
        return True

    def upsert(self, con, table, row, overwrite=True):
        info = list(con.execute('PRAGMA table_info(' + qi(table) + ')'))
        cols = [r['name'] for r in info if r['name'] in row]
        keys = [r['name'] for r in sorted(info, key=lambda r: r['pk']) if r['pk']]
        if not cols or not keys or not set(keys) <= row.keys():
            return False
        where = ' AND '.join(qi(k) + '=?' for k in keys)
        old = con.execute('SELECT * FROM ' + qi(table) + ' WHERE ' + where,
                          [row[k] for k in keys]).fetchone()
        if old and (not overwrite or all(old[k] == row[k] for k in cols)):
            return False
        if old:
            con.execute('UPDATE ' + qi(table) + ' SET ' + ','.join(qi(k) + '=?' for k in cols)
                        + ' WHERE ' + where, [row[k] for k in cols] + [row[k] for k in keys])
        else:
            con.execute('INSERT INTO ' + qi(table) + '(' + ','.join(map(qi, cols))
                        + ') VALUES(' + ','.join('?' for _ in cols) + ')', [row[k] for k in cols])
        return True

    def projection_valid(self, source, thread, length):
        path = source / 'thread_history_1.sqlite'
        if not path.is_file():
            return False
        with closing(connect(path)) as con:
            states = rows(con, 'thread_history_projection_state', thread)
            return bool(states) and all(r.get('next_rollout_byte_offset', 0) <= length for r in states)

    def copy_projection(self, source, target, thread, length, valid=True):
        src, dst = source / 'thread_history_1.sqlite', target / 'thread_history_1.sqlite'
        if not src.is_file() or not dst.is_file():
            return
        self.backup(target, dst, True)
        # Derived projection rows are immutable at a rollout prefix except for
        # in-progress items; copy only from the selected complete rollout owner.
        with closing(connect(src)) as a, closing(connect(dst, True)) as b, b:
            if not valid:
                for table in ('thread_turns', 'thread_items', 'thread_realtime_items', 'thread_history_projection_state'):
                    if 'thread_id' in columns(b, table):
                        b.execute('DELETE FROM ' + qi(table) + ' WHERE thread_id=?', (thread,))
                return
            a.execute('BEGIN')
            for state in rows(a, 'thread_history_projection_state', thread):
                if state.get('next_rollout_byte_offset', 0) > length:
                    raise BlockingIOError('Source projection advanced beyond the copied rollout')
            for table in ('thread_turns', 'thread_items', 'thread_realtime_items', 'thread_history_projection_state'):
                for row in rows(a, table, thread):
                    self.upsert(b, table, row)

    def catalog(self, root, row):
        path = root / 'sqlite' / 'codex-dev.db'
        if not path.is_file():
            return
        self.backup(root, path, True)
        with closing(connect(path, True)) as con, con:
            if not columns(con, 'local_thread_catalog'):
                return
            local_hosts = [r[0] for r in con.execute("SELECT host_id FROM local_thread_catalog_hosts WHERE host_kind='local'")]
            for host in local_hosts or ['local']:
                if row.get('archived') or internal_thread(row):
                    changed = con.execute('DELETE FROM local_thread_catalog WHERE host_id=? AND thread_id=?', (host, row['id'])).rowcount
                else:
                    changed = self.upsert(con, 'local_thread_catalog', {
                        'host_id': host, 'thread_id': row['id'],
                        'display_title': row.get('name') or row.get('title', ''),
                        'source_created_at': row.get('created_at_ms', 0) / 1000 if row.get('created_at_ms') else row.get('created_at', 0),
                        'source_updated_at': row.get('updated_at_ms', 0) / 1000 if row.get('updated_at_ms') else row.get('updated_at', 0),
                        'source_recency_at': (row.get('recency_at_ms') or row.get('updated_at_ms') or row.get('updated_at', 0) * 1000) / 1000,
                        'cwd': row.get('cwd'), 'source_kind': row.get('source') if row.get('source') in ('cli', 'vscode', 'exec', 'appServer') else 'vscode', 'source_detail': None,
                        'model_provider': row.get('model_provider'), 'git_branch': row.get('git_branch'),
                        'observation_sequence': 0, 'missing_candidate': 0,
                        'thread_source': row.get('thread_source'), 'project_id': row.get('project_id')})
                if changed:
                    self.report['catalog'] += 1
                    con.execute('UPDATE local_thread_catalog_metadata SET catalog_revision=catalog_revision+1 WHERE id=1')

    def clean_internal_catalog(self, root, threads):
        # Older replicas may already have published these internal records.
        # Remove only local sidebar entries, preserving thread rows, rollouts,
        # parent/child links and paginated history for the owning conversation.
        path = root / 'sqlite' / 'codex-dev.db'
        if not path.is_file():
            return
        self.backup(root, path, True)
        with closing(connect(path, True)) as con, con:
            if not columns(con, 'local_thread_catalog'):
                return
            removed = 0
            for row in threads.values():
                if internal_thread(row):
                    removed += con.execute("DELETE FROM local_thread_catalog WHERE thread_id=? AND host_id IN (SELECT host_id FROM local_thread_catalog_hosts WHERE host_kind='local')", (row['id'],)).rowcount
            if removed:
                con.execute('UPDATE local_thread_catalog_metadata SET catalog_revision=catalog_revision+1 WHERE id=1')
                self.report['catalog'] += removed

    def run(self, homes):
        homes = list(dict.fromkeys(plain(Path(p)) for p in homes))
        if any(a != b and (a in b.parents or b in a.parents) for a in homes for b in homes):
            raise ValueError('History homes overlap')
        homes = [p for p in homes if (p / 'state_5.sqlite').is_file()]
        self.report['homes'] = len(homes)
        candidates, existing = {}, {}
        for root in homes:
            with closing(connect(root / 'state_5.sqlite')) as con:
                existing[root] = {r['id']: r for r in rows(con, 'threads')}
            self.clean_internal_catalog(root, existing[root])
            for thread, row in existing[root].items():
                try:
                    path, relative = rollout_path(root, row)
                    if not path.is_file():
                        continue
                    fp = self.fingerprint(path)
                    candidates.setdefault(thread, []).append((root, row, path, relative, fp))
                except (OSError, ValueError) as error:
                    self.report['deferred'].append({'thread': thread, 'home': root.name, 'reason': str(error)})
        for thread, group in candidates.items():
            try:
                winner = max(group, key=lambda v: (v[4]['meaningful'], v[4]['size'], v[1].get('updated_at_ms') or v[1].get('updated_at', 0) * 1000))
                source, row, path, relative, fp = winner
                data = None
                divergent = False
                for other in group:
                    if other[4]['hash'] == fp['hash']:
                        continue
                    if data is None:
                        data = snapshot(path)
                    if replace_offset(snapshot(other[2]), data) is None:
                        divergent = True
                if divergent:
                    self.report['conflicts'].append({'thread': thread, 'homes': [g[0].name for g in group]})
                    continue
                valid_projection = self.projection_valid(source, thread, fp['size'])
                for target in homes:
                    old = existing[target].get(thread)
                    destination = rollout_path(target, old)[0] if old else target / relative
                    imported = dict(row, rollout_path=str(destination))
                    if not valid_projection and 'history_mode' in imported:
                        imported['history_mode'] = 'legacy'
                    needs_file = not destination.is_file() or self.fingerprint(destination)['hash'] != fp['hash']
                    needs_row = old is None or any(old.get(k) != v for k, v in imported.items())
                    completion_key = 'applied:' + str(destination)
                    pending = self.cache.get(completion_key) != fp['hash']
                    if not needs_file and not needs_row and not pending:
                        continue
                    try:
                        if needs_file:
                            if data is None:
                                data = snapshot(path)
                            if hashlib.sha256(data).hexdigest() != fp['hash']:
                                raise BlockingIOError('Source advanced; retry on next pass')
                            if self.publish(target, destination, data):
                                self.report['rollouts'] += 1
                        db = target / 'state_5.sqlite'
                        self.backup(target, db, True)
                        # Copy related project/section metadata before the thread;
                        # existing local project preferences win on ID collision.
                        with closing(connect(source / 'state_5.sqlite')) as a, closing(connect(db, True)) as b, b:
                            current = b.execute('SELECT * FROM threads WHERE id=?', (thread,)).fetchone()
                            if (dict(current) if current else None) != old:
                                raise BlockingIOError('Thread metadata changed during synchronization')
                            for table, id_col, identifier in (('projects', 'id', row.get('project_id')),
                                                               ('thread_sections', 'id', row.get('thread_section_id'))):
                                if identifier and columns(a, table):
                                    record = a.execute('SELECT * FROM ' + qi(table) + ' WHERE ' + qi(id_col) + '=?', (identifier,)).fetchone()
                                    if record:
                                        self.upsert(b, table, dict(record), False)
                            if row.get('project_id') and columns(a, 'project_roots'):
                                for record in a.execute('SELECT * FROM project_roots WHERE project_id=?', (row['project_id'],)):
                                    self.upsert(b, 'project_roots', dict(record), False)
                            self.upsert(b, 'threads', imported)
                            for table in ('thread_dynamic_tools', 'thread_attachments', 'thread_spawn_edges'):
                                if table == 'thread_spawn_edges':
                                    records = [dict(r) for r in a.execute('SELECT * FROM thread_spawn_edges WHERE child_thread_id=?', (thread,))] if columns(a, table) else []
                                else:
                                    records = rows(a, table, thread)
                                for record in records:
                                    self.upsert(b, table, record)
                        if (source != target or not valid_projection) and (needs_file or needs_row or pending):
                            self.copy_projection(source, target, thread, fp['size'], valid_projection)
                        self.catalog(target, imported)
                        self.cache[completion_key] = fp['hash']
                        self.report['threads'] += 1
                    except (OSError, ValueError, sqlite3.Error) as error:
                        self.report['deferred'].append({'thread': thread, 'home': target.name, 'reason': str(error)})
            except (OSError, ValueError, sqlite3.Error) as error:
                self.report['deferred'].append({'thread': thread, 'reason': str(error)})
        all_ids = set().union(*(set(v) for v in existing.values())) if existing else set()
        self.report['missing'] = sorted(all_ids - candidates.keys())
        atomic_json(self.cache_file, self.cache)
        atomic_json(self.directory / 'status.json', self.report)
        return self.report


def synchronize(homes, state_dir):
    with lock_file(Path(state_dir) / 'sync.lock'):
        if (Path(state_dir) / 'shared.json').exists():
            return {'ok': True, 'storage': 'shared', 'skipped': True}
        return Replicator(state_dir).run(homes)


if __name__ == '__main__':
    try:
        request = json.load(sys.stdin)
        result = synchronize(request['homes'], request['state_dir'])
        print(json.dumps(result, ensure_ascii=False))
    except Exception as error:
        print(json.dumps({'ok': False, 'message': str(error)}, ensure_ascii=False))
        sys.exit(1)
