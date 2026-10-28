"""Copy local Codex history into an empty private CODEX_HOME.

SQLite online backups include committed WAL pages. Rollouts remain byte copies;
only copied database/index/project metadata has paths and local hosts relocated.
Auth, config, provider settings and browser profiles are never import inputs.
"""
from contextlib import closing
import hashlib
import gzip
import json
import ntpath
import os
import pathlib
import re
import shutil
import sqlite3
import stat
import sys
import time
import uuid

DATABASE_NAME = re.compile(r'(?:state_\d+|thread_history_\d+|thread_catalog(?:_\d+)?)\.sqlite', re.I)
ROLLOUT_SUFFIXES = ('.jsonl', '.jsonl.zst', '.jsonl.gz')
ROLLOUT_DIRS = ('sessions', 'archived_sessions')
INDEX_FILES = ('session_index.jsonl', 'history.jsonl')
STATE_FILE = '.codex-global-state.json'
MARKER_FILE = '.agentcockpit-history.json'
STATE_KEYS = frozenset({
    'active-workspace-roots', 'saved-workspace-roots', 'workspace-root-order',
    'workspace-root-labels', 'electron-saved-workspace-roots', 'local-projects',
    'pinned-project-ids', 'project-order', 'project-appearances', 'selected-project',
    'sidebar-project-thread-orders', 'thread-project-assignments',
    'thread-project-membership-host-ids', 'projectless-thread-ids',
    'pinned-thread-ids', 'thread-titles', 'thread-workspace-root-hints',
    'thread-writable-roots', 'thread-projectless-output-directories',
    'electron-thread-read-state-v1', 'app-server-projects-migration-by-host',
    'app-server-project-id-by-legacy-project-id-by-host',
    'app-server-migrated-pinned-thread-ids-by-host',
    'app-server-pending-project-deletions-by-host',
    'electron-local-data-migration-id', 'electron-completed-local-data-migration-ids',
    'remote-project-connection-backfill-completed',
    'electron-workspace-root-labels', 'project-writable-roots', 'project-files',
    'hidden-project-ids', 'archived-project-ids', 'deleted-project-ids',
})
ATOM_KEYS = frozenset({
    'sidebar-custom-sections-v3', 'sidebar-collapsed-sections-v1',
    'flat-project-sidebar-preferences-v1', 'sidebar-project-list-expanded-v1',
    'thread-descriptions-v1',
    'unified-sidebar-project-order-v1',
})
SECRET_FIELDS = frozenset({
    'auth', 'authentication', 'authorization', 'config', 'credentials', 'cookies',
    'apikey', 'accesstoken', 'refreshtoken', 'idtoken', 'token', 'password', 'secret',
})
HOST_COLUMNS = frozenset({'host_id', 'local_host_id', 'source_host_id'})
PATH_COLUMNS = frozenset({
    'rollout_path', 'rollout_file_path', 'path', 'cwd', 'working_directory',
    'workspace_root', 'root_path', 'codex_home', 'home_path',
})
RELOCATED_COLUMNS = frozenset({'rollout_path', 'rollout_file_path', 'codex_home', 'home_path'})
HISTORY_TABLES = frozenset({
    'threads', 'thread_items', 'projects', 'sessions', 'messages', 'conversations',
    'thread_history', 'thread_catalog', 'local_thread_catalog', 'thread_timeline_ledger',
    'thread_turns', 'thread_realtime_items', 'thread_history_projection_state',
})
REAL_STATE_KEYS = frozenset({
    'local-projects', 'pinned-project-ids', 'project-order', 'pinned-thread-ids',
    'projectless-thread-ids', 'thread-titles', 'sidebar-project-thread-orders',
    'thread-project-assignments', 'thread-project-membership-host-ids',
})
CATALOG_TABLES = frozenset({
    'local_thread_catalog', 'local_thread_catalog_hosts', 'local_thread_catalog_metadata',
    'local_thread_catalog_sync_state', 'local_thread_catalog_scan_checkpoints',
    'local_thread_catalog_scan_entries', 'thread_timeline_ledger', 'codex_schema_migrations',
})
TASK_TABLES = frozenset({
    'agent_jobs', 'agent_job_items', 'agent_job_states', 'automations',
    'automation_runs', 'background_jobs', 'background_job_queue', 'task_queue',
})


def is_link(path):
    try:
        info = pathlib.Path(path).lstat()
    except FileNotFoundError:
        return False
    return stat.S_ISLNK(info.st_mode) or bool(
        getattr(info, 'st_file_attributes', 0) & getattr(stat, 'FILE_ATTRIBUTE_REPARSE_POINT', 0x400))


def ordinary(path):
    path = pathlib.Path(path)
    if not path.is_absolute() or '..' in path.parts:
        raise ValueError('History paths must be absolute and cannot traverse parents')
    for part in [path, *path.parents]:
        if is_link(part):
            raise ValueError('History path contains a directory/file link')
    return path


def key(path):
    value = str(path)
    return ntpath.normcase(ntpath.normpath(value)) if ntpath.splitdrive(value)[0] or os.name == 'nt' else os.path.normcase(os.path.normpath(value))


def quote(identifier):
    return '"' + identifier.replace('"', '""') + '"'


def readonly(path):
    ordinary(path)
    for suffix in ('-wal', '-shm'):
        ordinary(pathlib.Path(str(path) + suffix))
    con = sqlite3.connect(pathlib.Path(path).as_uri() + '?mode=ro', uri=True, timeout=10)
    con.execute('PRAGMA query_only=ON')
    return con


def tables(con):
    return [row[0] for row in con.execute("SELECT name FROM sqlite_master WHERE type='table'")]


def columns(con, table):
    return [row[1] for row in con.execute('PRAGMA table_info(' + quote(table) + ')')]


def walk_files(base):
    ordinary(base)
    for parent, directories, files in os.walk(base, followlinks=False):
        for name in directories:
            ordinary(pathlib.Path(parent) / name)
        for name in files:
            yield pathlib.Path(parent) / name


def database_paths(root):
    root = pathlib.Path(root)
    found = [path for path in root.iterdir() if DATABASE_NAME.fullmatch(path.name)]
    nested = root / 'sqlite'
    if nested.exists() or is_link(nested):
        ordinary(nested)
        if not nested.is_dir():
            raise ValueError('History SQLite directory is not a directory')
        found.extend(path for path in walk_files(nested)
                     if DATABASE_NAME.fullmatch(path.name) or path == nested / 'codex-dev.db')
    for path in sorted(found):
        ordinary(path)
        if not path.is_file():
            raise ValueError('History database is not a regular file')
        yield path


def history_count(root):
    root, count, primary = pathlib.Path(root), 0, None
    for path in database_paths(root):
        with closing(readonly(path)) as con:
            if 'threads' in tables(con):
                current = con.execute('SELECT COUNT(*) FROM threads').fetchone()[0]
                count = max(count, current)
                version = re.fullmatch(r'state_(\d+)\.sqlite', path.name, re.I)
                if path.parent == root and version and (primary is None or int(version[1]) > primary[0]):
                    primary = (int(version[1]), current)
    return primary[1] if primary is not None else count


def existing_history(root):
    # Official login can initialize empty databases and non-history UI state.
    # Those are replaceable; real records always preserve the complete target.
    for database in database_paths(root):
        with closing(readonly(database)) as con:
            for table in set(tables(con)) & HISTORY_TABLES:
                if con.execute('SELECT 1 FROM ' + quote(table) + ' LIMIT 1').fetchone():
                    return True
    for name in INDEX_FILES:
        path = root / name
        if path.exists() or is_link(path):
            ordinary(path)
            if not path.is_file():
                return True
            with path.open(encoding='utf-8-sig') as file:
                if any(line.strip() for line in file):
                    return True
    state_file = root / STATE_FILE
    if state_file.exists() or is_link(state_file):
        state = read_state(state_file)
        if any(state.get(name) for name in REAL_STATE_KEYS):
            return True
    marker = root / MARKER_FILE
    if marker.exists() or is_link(marker):
        value = read_state(marker)
        if any(value.get(name, 0) for name in ('threads', 'projects', 'rollouts')):
            return True
    for name in ROLLOUT_DIRS:
        base = root / name
        if base.exists() or is_link(base):
            ordinary(base)
            if not base.is_dir():
                return True
            if next(walk_files(base), None) is not None:
                return True
    return False


def read_state(path):
    ordinary(path)
    if not path.is_file():
        raise ValueError('History metadata is not a regular file')
    if not path.stat().st_size:
        return {}
    with path.open(encoding='utf-8-sig') as file:
        value = json.load(file)
    if not isinstance(value, dict):
        raise ValueError('History project metadata is not an object')
    return value


def initialized_files(root):
    files = list(database_paths(root))
    for database in list(files):
        wal = pathlib.Path(str(database) + '-wal')
        if wal.exists() or is_link(wal):
            ordinary(wal)
            files.append(wal)
    for name in (*INDEX_FILES, STATE_FILE, MARKER_FILE):
        path = root / name
        if path.exists() or is_link(path):
            ordinary(path)
            files.append(path)
    fingerprints = {}
    for path in files:
        ordinary(path)
        digest = hashlib.sha256()
        with path.open('rb') as file:
            for block in iter(lambda: file.read(1024 * 1024), b''):
                digest.update(block)
        fingerprints[path.relative_to(root)] = digest.digest()
    return fingerprints


def relative_home(value, source):
    """Recognize either slash spelling of a native/Windows CODEX_HOME path."""
    old, home = str(value).replace('\\', '/'), str(source).replace('\\', '/').rstrip('/')
    if old.startswith('//?/'):
        old = old[4:]
    if home.startswith('//?/'):
        home = home[4:]
    compare = str.casefold if ntpath.splitdrive(home)[0] or os.name == 'nt' else lambda x: x
    if compare(old) == compare(home):
        return pathlib.Path('.')
    if not compare(old).startswith(compare(home) + '/'):
        return None
    parts = pathlib.PurePosixPath(old[len(home) + 1:]).parts
    if '..' in parts or pathlib.PurePosixPath(*parts).is_absolute():
        raise ValueError('A history path traverses outside its home')
    return pathlib.Path(*parts)


def make_relocator(source, target):
    def text(value, relocate_path=False):
        if value.startswith('local:'):
            return 'local:' + str(target) if key(value[6:]) == key(source) else value
        if relocate_path:
            rel = relative_home(value, source)
            return str(target / rel) if rel is not None else value
        return value

    def relocate(value, field=''):
        if isinstance(value, dict):
            return {text(k): relocate(v, k) for k, v in value.items()}
        if isinstance(value, list):
            return [relocate(v, field) for v in value]
        return text(value, field in RELOCATED_COLUMNS) if isinstance(value, str) else value
    return text, relocate


def history_state(path, relocate):
    state = read_state(path)

    def scrub(value):
        if isinstance(value, dict):
            return {k: scrub(v) for k, v in value.items()
                    if re.sub(r'[-_]', '', k).casefold() not in SECRET_FIELDS}
        return [scrub(v) for v in value] if isinstance(value, list) else value

    selected = {k: v for k, v in state.items() if k in STATE_KEYS}
    # Completed migrations are part of the history format. Without them the
    # desktop replays its 2026-07-13 local-project migration and turns obsolete
    # workspace suggestions into new projects. Only an already-migrated source
    # has an authoritative project set; do not guess for an older source.
    selected = project_visibility_state(selected)
    atoms = state.get('electron-persisted-atom-state', {})
    if isinstance(atoms, dict):
        keep = {k: v for k, v in atoms.items() if k in ATOM_KEYS or k.startswith('sidebar-project-expanded-v1-')}
        if keep:
            selected['electron-persisted-atom-state'] = keep
    return relocate(scrub(selected))


LOCAL_PROJECT_MIGRATION = '2026-07-13-local-projects'
VISIBILITY_REPAIR_VERSION = 2


def project_visibility_state(state):
    state = dict(state)
    projects = state.get('local-projects')
    completed = state.get('electron-completed-local-data-migration-ids', [])
    if LOCAL_PROJECT_MIGRATION not in completed or not isinstance(projects, dict):
        return state
    active_paths = {key(path) for p in projects.values() if isinstance(p, dict)
                    for path in p.get('rootPaths', []) if isinstance(path, str)}
    for field in ('electron-saved-workspace-roots', 'saved-workspace-roots', 'workspace-root-order'):
        if isinstance(state.get(field), list):
            state[field] = [p for p in state[field] if isinstance(p, str) and key(p) in active_paths]
    return state


def primary_state_db(root):
    candidates = [p for p in database_paths(root) if p.parent == root and re.fullmatch(r'state_\d+\.sqlite', p.name, re.I)]
    return max(candidates, key=lambda p: int(re.search(r'\d+', p.name)[0])) if candidates else None


def _project_rows(root):
    database = primary_state_db(root)
    if database is None:
        return {}, set(), None
    with closing(readonly(database)) as con:
        names = set(tables(con))
        if 'projects' not in names or 'project_roots' not in names:
            return {}, set(), database
        needed = {'id', 'name', 'metadata', 'created_at_ms'}
        if not needed <= set(columns(con, 'projects')):
            return {}, set(), database
        projects = {}
        for identifier, name, metadata, created in con.execute('SELECT id,name,metadata,created_at_ms FROM projects'):
            roots = [row[0] for row in con.execute('SELECT path FROM project_roots WHERE project_id=? ORDER BY position', (identifier,))]
            projects[identifier] = {'name': name, 'metadata': metadata, 'created': created, 'roots': roots}
        used = set()
        if 'threads' in names and 'project_id' in columns(con, 'threads'):
            used.update(row[0] for row in con.execute('SELECT DISTINCT project_id FROM threads WHERE project_id IS NOT NULL'))
    return projects, used, database


def project_repair_plan(source, target):
    """Identify only the first-launch legacy migration artifacts of a copy."""
    source, target = ordinary(source).resolve(), ordinary(target).resolve()
    if source == target or source in target.parents or target in source.parents:
        raise ValueError('Source and target history directories overlap')
    marker = read_state(target / MARKER_FILE)
    if key(marker.get('source', '')) != key(source):
        raise ValueError('History copy ownership does not match the repair source')
    original, current = read_state(source / STATE_FILE), read_state(target / STATE_FILE)
    completed = original.get('electron-completed-local-data-migration-ids', [])
    base = original.get('local-projects')
    present = current.get('local-projects')
    if LOCAL_PROJECT_MIGRATION not in completed or not isinstance(base, dict) or not isinstance(present, dict):
        return {'eligible': False, 'legacy_ids': [], 'project_ids': [], 'uncertain': 0, 'message': '来源没有已完成的项目迁移记录，保留现有项目'}
    visible = {key(p) for v in base.values() if isinstance(v, dict) for p in v.get('rootPaths', []) if isinstance(p, str)}
    stale = {key(p) for p in original.get('electron-saved-workspace-roots', []) if isinstance(p, str) and key(p) not in visible}
    source_db, _, _ = _project_rows(source)
    target_db, referenced, database = _project_rows(target)
    if database is None or not target_db:
        return {'eligible': False, 'legacy_ids': [], 'project_ids': [], 'uncertain': 0, 'message': '无法核对目标项目与对话引用，保留现有项目'}
    assigned = current.get('thread-project-assignments', {})
    for value in assigned.values() if isinstance(assigned, dict) else []:
        if isinstance(value, dict) and value.get('projectId'):
            referenced.add(value['projectId'])
    protected = set(current.get('pinned-project-ids', []))
    selected = current.get('selected-project', {})
    if isinstance(selected, dict) and selected.get('projectId'):
        protected.add(selected['projectId'])
    created_at = float(marker.get('copied_at', 0)) * 1000
    candidates, modern, uncertain = [], set(), 0
    for identifier, value in present.items():
        if identifier in base or not isinstance(value, dict):
            continue
        roots = value.get('rootPaths', [])
        paths = {key(p) for p in roots if isinstance(p, str)}
        if not paths or not paths <= stale:
            continue
        created = value.get('createdAt', 0)
        updated = value.get('updatedAt', 0)
        canonical_name = pathlib.PureWindowsPath(roots[0]).name if ntpath.splitdrive(roots[0])[0] else pathlib.Path(roots[0]).name
        matching = [(pid, project) for pid, project in target_db.items()
                    if pid not in source_db and {key(p) for p in project['roots']} == paths]
        safe = (identifier.startswith('local-') and identifier not in referenced | protected
                and isinstance(created, (int, float)) and isinstance(updated, (int, float))
                and created_at - 30000 <= created <= created_at + 180000
                and abs(updated - created) <= 1000 and value.get('name') == canonical_name)
        for pid, project in matching:
            safe = safe and pid not in referenced | protected and project['name'] == value.get('name')
            safe = safe and project['metadata'] in ('{}', None, '') and created <= project['created'] <= created + 180000
        if safe:
            candidates.append(identifier)
            modern.update(pid for pid, _ in matching)
        else:
            uncertain += 1
    return {'eligible': True, 'legacy_ids': candidates, 'project_ids': sorted(modern), 'uncertain': uncertain,
            'legacy_count': len(candidates), 'project_count': len(modern),
            'message': '已识别旧工作区列表自动重建的无对话项目'}


def _atomic_bytes(path, data):
    ordinary(path)
    temporary = path.with_name('.project-repair-' + uuid.uuid4().hex + '.tmp')
    try:
        with temporary.open('xb') as output:
            output.write(data)
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary, path)
    finally:
        if temporary.exists():
            temporary.unlink()


def repair_projects(source, target, dry_run=True, expected_plan=None):
    """Offline repair. The manager must confirm this target client is closed."""
    source, target = ordinary(source).resolve(), ordinary(target).resolve()
    plan = project_repair_plan(source, target)
    if dry_run or not plan['eligible']:
        return plan
    if expected_plan is not None and any(plan.get(k) != expected_plan.get(k) for k in ('legacy_ids', 'project_ids')):
        raise ValueError('Project metadata changed after repair was queued; review it again')
    state_file = target / STATE_FILE
    original_bytes = state_file.read_bytes()
    current = json.loads(original_bytes)
    original = read_state(source / STATE_FILE)
    remove_legacy, remove_modern = set(plan['legacy_ids']), set(plan['project_ids'])
    remove_ids = remove_legacy | remove_modern
    local_projects = current.get('local-projects', {})
    current['local-projects'] = {k: v for k, v in local_projects.items() if k not in remove_legacy}
    for field in ('project-order', 'pinned-project-ids'):
        if isinstance(current.get(field), list):
            current[field] = [v for v in current[field] if v not in remove_ids]
    for field in ('project-appearances', 'sidebar-project-thread-orders'):
        if isinstance(current.get(field), dict):
            current[field] = {k: v for k, v in current[field].items() if k not in remove_ids}
    for hosts in (current.get('app-server-project-id-by-legacy-project-id-by-host', {}),):
        if isinstance(hosts, dict):
            for host, mapping in list(hosts.items()):
                if isinstance(mapping, dict):
                    hosts[host] = {k: v for k, v in mapping.items() if k not in remove_legacy and not (isinstance(v, str) and v in remove_modern)}
    for field in ('electron-local-data-migration-id', 'electron-completed-local-data-migration-ids', 'remote-project-connection-backfill-completed'):
        if field in original:
            # Preserve newer migration completion records added by this client.
            current[field] = list(dict.fromkeys([*(current.get(field) or []), *original[field]])) if field.endswith('migration-ids') else current.get(field) if current.get(field) is not None else original[field]
    current = project_visibility_state(current)
    next_bytes = json.dumps(current, ensure_ascii=False).encode('utf-8')
    backup = target / '.project-repair-backups' / uuid.uuid4().hex
    ordinary(backup)
    backup.mkdir(parents=True)
    (backup / STATE_FILE).write_bytes(original_bytes)
    database = primary_state_db(target)
    with closing(readonly(database)) as origin, closing(sqlite3.connect(backup / database.name)) as copied:
        origin.backup(copied)
    with closing(sqlite3.connect(database)) as con:
        con.execute('PRAGMA foreign_keys=ON')
        con.execute('BEGIN IMMEDIATE')
        state_written = False
        try:
            if state_file.read_bytes() != original_bytes:
                raise ValueError('Target project state changed; repair was not applied')
            for identifier in remove_modern:
                if con.execute('SELECT 1 FROM threads WHERE project_id=? LIMIT 1', (identifier,)).fetchone():
                    raise ValueError('A project gained a conversation; its registration was retained')
                for table in ('project_roots', 'project_idempotency_keys'):
                    if table in tables(con):
                        con.execute('DELETE FROM '+quote(table)+' WHERE project_id=?', (identifier,))
                con.execute('DELETE FROM projects WHERE id=?', (identifier,))
            _atomic_bytes(state_file, next_bytes)
            state_written = True
            con.commit()
        except BaseException:
            con.rollback()
            if state_written and state_file.read_bytes() == next_bytes:
                _atomic_bytes(state_file, original_bytes)
            raise
    result = {**plan, 'applied': True, 'backup': str(backup), 'repaired_at': time.time(), 'visibility_version': VISIBILITY_REPAIR_VERSION}
    _atomic_bytes(target / '.agentcockpit-project-visibility.json', json.dumps(result, ensure_ascii=False).encode('utf-8'))
    return result


def _publish_file(source, destination):
    # Windows rename refuses an existing destination; POSIX rename overwrites,
    # so publish via an exclusive hard link there. Both stay on the staging FS.
    if os.name == 'nt':
        os.rename(source, destination)
    else:
        os.link(source, destination)
        source.unlink()


def copy_history(source, target):
    source, target = ordinary(source), ordinary(target)
    source, target = source.resolve(), target.resolve()
    if source == target or source in target.parents or target in source.parents:
        raise ValueError('Source and target history directories overlap')
    if not source.is_dir() or not target.is_dir():
        raise ValueError('History directory is missing')
    if existing_history(target):
        return {'threads': history_count(target), 'preserved_existing': True}
    initialized = initialized_files(target)
    stage = target / ('.history-stage-' + uuid.uuid4().hex)
    stage.mkdir()
    deadline = time.monotonic() + 180
    published, created_directories, backed_up = [], [], []
    backup = target / ('.history-before-copy-' + uuid.uuid4().hex)
    retain_stage = False

    def budget():
        if time.monotonic() > deadline:
            raise TimeoutError('History copy timed out; source history was retained')

    def copy_bytes(src, dst):
        ordinary(src)
        dst.parent.mkdir(parents=True, exist_ok=True)
        for attempt in range(3):
            budget()
            with src.open('rb') as origin, dst.open('wb') as output:
                before = os.fstat(origin.fileno())
                remaining = before.st_size
                while remaining:
                    budget()
                    block = origin.read(min(1024 * 1024, remaining))
                    if not block:
                        raise ValueError('A history file changed during its snapshot')
                    output.write(block)
                    remaining -= len(block)
                after = os.fstat(origin.fileno())
            if (before.st_size, before.st_mtime_ns) == (after.st_size, after.st_mtime_ns):
                shutil.copystat(src, dst, follow_symlinks=False)
                return
        raise ValueError('A history file is still changing; retry the snapshot')

    text, relocate = make_relocator(source, target)
    try:
        for src in database_paths(source):
            budget()
            dst = stage / src.relative_to(source)
            dst.parent.mkdir(parents=True, exist_ok=True)
            with closing(readonly(src)) as origin, closing(sqlite3.connect(dst)) as dest:
                origin.backup(dest, pages=256, progress=lambda *_: budget(), sleep=0.05)
                dest.commit()
        copied, rollout_by_name = 0, {}
        for directory in ROLLOUT_DIRS:
            base = source / directory
            if not base.exists() and not is_link(base):
                continue
            ordinary(base)
            if not base.is_dir():
                raise ValueError('History sessions path is not a directory')
            for src in walk_files(base):
                budget()
                if not src.name.lower().endswith(ROLLOUT_SUFFIXES):
                    continue
                rel = src.relative_to(source)
                copy_bytes(src, stage / rel)
                rollout_by_name.setdefault(os.path.normcase(src.name), []).append(rel)
                copied += 1
        restored_compressed = 0
        for directory in ROLLOUT_DIRS:
            base = stage / directory
            if not base.is_dir():
                continue
            for compressed in list(walk_files(base)):
                if not compressed.name.lower().endswith(('.jsonl.zst', '.jsonl.gz')):
                    continue
                plain = compressed.with_suffix('')
                if plain.exists():
                    continue
                budget()
                if compressed.suffix.lower() == '.gz':
                    opener = gzip.open
                else:
                    try:
                        from compression import zstd
                        opener = zstd.open
                    except ImportError:
                        try:
                            import zstandard
                            opener = zstandard.open
                        except ImportError as error:
                            raise ValueError('Compressed history requires Python zstd support') from error
                with opener(compressed, 'rb') as origin, plain.open('xb') as output:
                    while True:
                        budget()
                        block = origin.read(1024 * 1024)
                        if not block:
                            break
                        output.write(block)
                rollout_by_name.setdefault(os.path.normcase(plain.name), []).append(plain.relative_to(stage))
                restored_compressed += 1
        for name in INDEX_FILES:
            src = source / name
            if src.exists() or is_link(src):
                ordinary(src)
                copied_index = stage / name
                copy_bytes(src, copied_index)
                rewritten = stage / (name + '.rewritten')
                with copied_index.open(encoding='utf-8-sig') as file, rewritten.open('w', encoding='utf-8', newline='\n') as output:
                    for line in file:
                        budget()
                        if not line.strip():
                            continue
                        row = json.loads(line)
                        if isinstance(row, dict):
                            row = {k: (text(v, k in RELOCATED_COLUMNS) if k in HOST_COLUMNS | PATH_COLUMNS and isinstance(v, str) else v)
                                   for k, v in row.items()}
                        output.write(json.dumps(row, ensure_ascii=False) + '\n')
                os.replace(rewritten, copied_index)
        state_path = source / STATE_FILE
        if state_path.exists() or is_link(state_path):
            state = history_state(state_path, relocate)
            if state:
                previous = read_state(target / STATE_FILE) if (target / STATE_FILE).exists() else {}
                for name, value in state.items():
                    if not previous.get(name):
                        previous[name] = value
                (stage / STATE_FILE).write_text(json.dumps(previous, ensure_ascii=False), encoding='utf-8')

        # Old nested state databases can disagree about archival status. The
        # newest root database is authoritative; keep its dangling archived
        # records without inventing conversation contents or deleting indexes.
        roots = [p for p in database_paths(stage) if p.parent == stage and re.fullmatch(r'state_\d+\.sqlite', p.name, re.I)]
        authoritative = max(roots, key=lambda p: int(re.search(r'\d+', p.name)[0])) if roots else None
        archived_names, missing_archived = set(), set()
        if authoritative:
            with closing(sqlite3.connect(authoritative)) as con:
                if 'threads' in tables(con) and {'archived', 'rollout_path'} <= set(columns(con, 'threads')):
                    for (path,) in con.execute('SELECT rollout_path FROM threads WHERE archived=1 AND rollout_path IS NOT NULL'):
                        archived_names.add(os.path.normcase(str(path).replace('\\', '/').rsplit('/', 1)[-1]))

        def rollout_relative(old):
            rel = relative_home(old, source)
            name = os.path.normcase(str(old).replace('\\', '/').rsplit('/', 1)[-1])
            if rel is None or not (stage / rel).is_file():
                choices = rollout_by_name.get(name, [])
                if not choices and rel is not None and name in archived_names and rel.parts and rel.parts[0] in ROLLOUT_DIRS:
                    missing_archived.add(name)
                    return rel
                if len(choices) != 1:
                    raise ValueError('A thread rollout could not be copied completely')
                rel = choices[0]
            if '..' in rel.parts or not rel.parts or rel.parts[0] not in ROLLOUT_DIRS or not (stage / rel).is_file():
                raise ValueError('A thread rollout is missing from the private copy')
            return rel

        threads = projects = 0
        primary_counts = None
        for path in database_paths(stage):
            budget()
            with closing(sqlite3.connect(path)) as con:
                con.execute('PRAGMA trusted_schema=OFF')
                names = tables(con)
                clear = set(names) & TASK_TABLES
                if path.name == 'codex-dev.db':
                    # This sidecar mixes catalog data with active automations,
                    # account caches and settings. Keep schema, not those rows.
                    clear |= {table for table in names if table not in CATALOG_TABLES and not table.startswith('sqlite_')}
                if clear:
                    con.execute('PRAGMA secure_delete=ON')
                    for table in sorted(clear):
                        con.execute('DELETE FROM ' + quote(table))
                if 'threads' in names:
                    threads = max(threads, con.execute('SELECT COUNT(*) FROM threads').fetchone()[0])
                if 'projects' in names:
                    projects = max(projects, con.execute('SELECT COUNT(*) FROM projects').fetchone()[0])
                version = re.fullmatch(r'state_(\d+)\.sqlite', path.name, re.I)
                if path.parent == stage and version and (primary_counts is None or int(version[1]) > primary_counts[0]):
                    primary_counts = (int(version[1]),
                        con.execute('SELECT COUNT(*) FROM threads').fetchone()[0] if 'threads' in names else 0,
                        con.execute('SELECT COUNT(*) FROM projects').fetchone()[0] if 'projects' in names else 0)
                for table in names:
                    if table.startswith('sqlite_'):
                        continue
                    for col in columns(con, table):
                        if table == 'thread_timeline_ledger' and col == 'payload_json':
                            qt, qc = quote(table), quote(col)
                            for (old,) in con.execute('SELECT DISTINCT '+qc+' FROM '+qt+' WHERE '+qc+' IS NOT NULL').fetchall():
                                if isinstance(old, str):
                                    new = json.dumps(relocate(json.loads(old)), ensure_ascii=False)
                                    con.execute('UPDATE '+qt+' SET '+qc+'=? WHERE '+qc+'=?', (new, old))
                            continue
                        if col not in HOST_COLUMNS | PATH_COLUMNS:
                            continue
                        qt, qc = quote(table), quote(col)
                        for (old,) in con.execute('SELECT DISTINCT ' + qc + ' FROM ' + qt + ' WHERE ' + qc + ' IS NOT NULL').fetchall():
                            budget()
                            if not isinstance(old, str) or not old:
                                continue
                            new = str(target / rollout_relative(old)) if col.startswith('rollout') else text(old, col in RELOCATED_COLUMNS)
                            if new != old:
                                con.execute('UPDATE ' + qt + ' SET ' + qc + '=? WHERE ' + qc + '=?', (new, old))
                con.commit()
                if con.execute('PRAGMA quick_check').fetchall() != [('ok',)]:
                    raise ValueError('Copied history database failed validation')
                con.execute('PRAGMA journal_mode=DELETE')
                if clear:
                    con.execute('VACUUM')
        if primary_counts is not None:
            _, threads, projects = primary_counts
        marker = {'source': str(source), 'threads': threads, 'projects': projects,
                  'rollouts': copied, 'restored_compressed': restored_compressed,
                  'missing_archived_rollouts': len(missing_archived), 'copied_at': time.time(),
                  'visibility_version': VISIBILITY_REPAIR_VERSION}
        (stage / MARKER_FILE).write_text(json.dumps(marker), encoding='utf-8')
        # Recheck after snapshotting, then publish each leaf without replacement.
        if existing_history(target):
            return {'threads': history_count(target), 'preserved_existing': True}
        if initialized_files(target) != initialized:
            raise ValueError('Target initialization files changed during the snapshot; retry with this account closed')
        files = sorted(walk_files(stage), key=lambda p: (p.name == MARKER_FILE, str(p)))
        staged = {item.relative_to(stage) for item in files}
        for relative in initialized:
            if DATABASE_NAME.fullmatch(relative.name) and relative not in staged:
                raise ValueError('Target has a different initialized history database version; an empty history home is required')
        for item in files:
            budget()
            rel = item.relative_to(stage)
            dest = target / rel
            ordinary(dest)
            missing = []
            parent = dest.parent
            while parent != target and not parent.exists():
                missing.append(parent)
                parent = parent.parent
            for parent in reversed(missing):
                parent.mkdir()
                created_directories.append(parent)
            if dest.exists():
                if rel not in initialized:
                    raise FileExistsError('A target history file appeared during publication')
                old = backup / rel
                old.parent.mkdir(parents=True, exist_ok=True)
                os.rename(dest, old)
                backed_up.append((dest, old))
            if DATABASE_NAME.fullmatch(dest.name) or dest.name == 'codex-dev.db':
                for suffix in ('-wal', '-shm'):
                    side = pathlib.Path(str(dest) + suffix)
                    if side.exists() or is_link(side):
                        ordinary(side)
                        old = backup / side.relative_to(target)
                        old.parent.mkdir(parents=True, exist_ok=True)
                        os.rename(side, old)
                        backed_up.append((side, old))
            _publish_file(item, dest)
            published.append((dest, item))
        return {**{k: marker[k] for k in ('threads', 'projects', 'rollouts', 'restored_compressed', 'missing_archived_rollouts', 'visibility_version')}, 'preserved_existing': False}
    except BaseException as error:
        rollback_errors = []
        for dest, item in reversed(published):
            try:
                ordinary(dest)
                if dest.exists():
                    os.replace(dest, item)
            except (OSError, ValueError) as rollback_error:
                rollback_errors.append(str(rollback_error))
        for dest, old in reversed(backed_up):
            try:
                ordinary(dest)
                os.replace(old, dest)
            except (OSError, ValueError) as rollback_error:
                rollback_errors.append(str(rollback_error))
        for directory in reversed(created_directories):
            try:
                directory.rmdir()
            except OSError as rollback_error:
                rollback_errors.append(str(rollback_error))
        if rollback_errors:
            retain_stage = True
            raise RuntimeError('History publication stopped; staging was retained for recovery: ' + str(stage)) from error
        raise
    finally:
        # Only remove this call's unique checked staging directory. No source,
        # credentials, existing target files or retained recovery files are removed.
        if not retain_stage and stage.exists() and stage.parent.resolve() == target and stage.name.startswith('.history-stage-'):
            ordinary(stage)
            shutil.rmtree(stage)
        if backup.exists() and not retain_stage:
            for parent, _, _ in os.walk(backup, topdown=False, followlinks=False):
                try:
                    pathlib.Path(parent).rmdir()
                except OSError:
                    pass  # Successful replacements retain their original files.


if __name__ == '__main__':
    try:
        request = json.load(sys.stdin)
        if request.get('mode') == 'repair_projects':
            result = repair_projects(request['source'], request['target'], request.get('dry_run', True), request.get('expected_plan'))
        else:
            result = copy_history(request['source'], request['target'])
        print(json.dumps({'ok': True, **result}))
    except Exception as error:
        print(json.dumps({'ok': False, 'message': str(error)}))
        sys.exit(1)
