from contextlib import closing
import importlib.util
import json
import os
from pathlib import Path
import queue
import sqlite3
import subprocess
import threading
import time
import unittest
from unittest import mock
import uuid

ROOT = Path(__file__).resolve().parents[1]


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


shared = load('shared_history', ROOT / 'src-tauri/scripts/chatgpt-history-shared.py')
fixture = load('shared_fixture', ROOT / 'tests/chatgpt-history-sync.test.py')


class SharedHistoryTests(unittest.TestCase):
    def setUp(self):
        self.fixture = fixture.HistoryReplication('runTest')
        self.fixture.setUp()
        self.addCleanup(self.fixture.doCleanups)
        self.homes = self.fixture.homes
        self.canonical = self.homes[0]
        self.request = {'canonical': str(self.canonical), 'homes': [str(h) for h in self.homes], 'state_dir': str(self.fixture.state)}

    def run_shared(self):
        result = shared.synchronize(self.request)
        for home in self.homes:
            self.assertEqual((home / 'auth.json').read_bytes(), b'private-' + home.name.encode())
            config = shared.tomllib.loads((home / 'config.toml').read_text(encoding='utf-8'))
            self.assertEqual(config.pop('sqlite_home', None), str(self.canonical) if home != self.canonical else None)
            self.assertEqual(config, {'provider': 'private'})
        return result

    def test_migration_keeps_union_and_one_physical_copy(self):
        self.fixture.put(self.homes[0], 'native-chat', ['native'])
        self.fixture.put(self.homes[1], 'private-chat', ['private'])
        result = self.run_shared()
        self.assertFalse(result['pending'])
        self.assertGreater(result['released_bytes'], 0)
        for home in self.homes[1:]:
            self.assertTrue(shared.exact_link(home / 'sessions', self.canonical / 'sessions'))
            self.assertTrue(shared.exact_link(home / 'thread-writer-locks', self.canonical / 'thread-writer-locks'))
            self.assertFalse((home / 'state_5.sqlite').exists())
            self.assertFalse((home / 'thread_history_1.sqlite').exists())
            self.assertFalse(list(home.glob('.history-shared-stage-*')))
            self.assertTrue(os.path.samefile(home / 'sessions/private-chat.jsonl', self.canonical / 'sessions/private-chat.jsonl'))
        with fixture.database(self.canonical / 'state_5.sqlite') as con:
            self.assertEqual(con.execute('SELECT COUNT(*) FROM threads').fetchone()[0], 2)
            for path, in con.execute('SELECT rollout_path FROM threads'):
                self.assertTrue(Path(path).is_relative_to(self.canonical))
        self.assertEqual(len(list((self.fixture.state / 'before-shared').glob('*/*.gz'))), 4)
        self.assertFalse(self.run_shared()['pending'])

    def test_changes_through_a_private_home_are_immediately_shared(self):
        self.fixture.put(self.canonical, 'chat', ['first'])
        self.run_shared()
        path = self.homes[1] / 'sessions/chat.jsonl'
        with path.open('ab') as file:
            file.write(b'{"text":"second"}\n')
        self.assertEqual(path.read_bytes(), (self.homes[2] / 'sessions/chat.jsonl').read_bytes())
        new = self.homes[2] / 'sessions/new-chat.jsonl'
        new.write_bytes(b'{"text":"new"}\n')
        self.assertTrue(os.path.samefile(new, self.canonical / 'sessions/new-chat.jsonl'))
        self.assertTrue((self.homes[1] / 'sessions/new-chat.jsonl').exists())
        new.unlink()
        self.assertFalse((self.canonical / 'sessions/new-chat.jsonl').exists())

    def test_native_open_file_handle_is_not_replaced(self):
        path = self.fixture.put(self.canonical, 'chat', ['first'])
        with path.open('ab') as writer:
            self.run_shared()
            writer.write(b'{"text":"still open"}\n')
            writer.flush()
            self.assertIn(b'still open', (self.homes[1] / 'sessions/chat.jsonl').read_bytes())

    def test_new_empty_profile_links_without_copying_history_databases(self):
        self.fixture.put(self.canonical, 'chat', ['first'])
        for home in self.homes[1:]:
            for name in ('state_5.sqlite', 'thread_history_1.sqlite'):
                (home / name).unlink()
        result = self.run_shared()
        self.assertFalse(result['pending'])
        self.assertEqual(result['released_bytes'], 0)
        self.assertEqual(len(list((self.canonical / 'sessions').glob('*.jsonl'))), 1)

    def test_conflicting_continuations_preserve_both_private_and_shared_data(self):
        a = self.fixture.put(self.canonical, 'chat', ['first', 'shared branch'])
        b = self.fixture.put(self.homes[1], 'chat', ['first', 'private branch'])
        before = a.read_bytes(), b.read_bytes()
        result = shared.synchronize(self.request)
        self.assertTrue(result['pending'])
        self.assertEqual((a.read_bytes(), b.read_bytes()), before)
        self.assertFalse((self.homes[1] / shared.MARKER).exists())
        self.assertTrue((self.homes[1] / 'state_5.sqlite').is_file())

    def test_directory_link_failure_rolls_back_credentials_config_and_directories(self):
        path = self.fixture.put(self.homes[1], 'chat', ['private'])
        original = shared.link_directory

        def fail_archive(link, target):
            if link.parent == self.homes[1] and link.name == 'archived_sessions':
                raise OSError('simulated junction failure')
            return original(link, target)

        with mock.patch.object(shared, 'link_directory', side_effect=fail_archive):
            result = shared.synchronize(self.request)
        self.assertTrue(result['pending'])
        self.assertIn(b'private', path.read_bytes())
        self.assertFalse(shared.exact_link(path.parent, self.canonical / 'sessions'))
        self.assertEqual((self.homes[1] / 'config.toml').read_bytes(), b'provider="private"')
        self.assertEqual((self.homes[1] / 'auth.json').read_bytes(), b'private-second')

    def test_live_private_profile_waits_until_it_is_closed(self):
        self.fixture.put(self.homes[1], 'chat', ['private'])
        self.request['busy_homes'] = [str(self.homes[1])]
        result = shared.synchronize(self.request)
        self.assertTrue(result['pending'])
        self.assertFalse((self.homes[1] / shared.MARKER).exists())
        self.request['busy_homes'] = []
        self.assertFalse(self.run_shared()['pending'])

    def test_deferred_merge_retains_old_data_and_leaves_account_unmigrated(self):
        self.fixture.put(self.homes[1], 'chat', ['private'])
        with mock.patch.object(fixture.sync.Replicator, 'run', return_value={'conflicts': [], 'deferred': [{'thread': 'chat'}]}):
            # shared loads its own replica module, so intercept the created instance.
            engine = shared.SharedHistory(self.canonical, self.fixture.state, fixture.sync)
            with mock.patch.object(engine.rep, 'run', return_value={'conflicts': [], 'deferred': [{'thread': 'chat'}]}):
                with self.assertRaisesRegex(ValueError, 'could not be merged'):
                    engine.migrate(self.homes[1])
        self.assertTrue((self.homes[1] / 'state_5.sqlite').exists())
        self.assertFalse((self.homes[1] / shared.MARKER).exists())
        self.assertTrue((self.homes[1] / 'sessions/chat.jsonl').exists())

    def test_active_migrated_profile_does_not_clean_interrupted_files(self):
        self.run_shared()
        private = self.homes[1]
        stage = private / '.history-shared-stage-sessions-test'
        stage.mkdir()
        (stage / 'still-writing.jsonl').write_text('retained', encoding='utf-8')
        marker = json.loads((private / shared.MARKER).read_text(encoding='utf-8'))
        marker['stages'] = {'sessions': stage.name}
        (private / shared.MARKER).write_text(json.dumps(marker), encoding='utf-8')
        self.request['busy_homes'] = [str(private)]
        shared.synchronize(self.request)
        self.assertTrue((stage / 'still-writing.jsonl').exists())
        self.request['busy_homes'] = []
        result = shared.synchronize(self.request)
        self.assertTrue(result['pending'])
        self.assertTrue((stage / 'still-writing.jsonl').exists())

    def test_compression_failure_does_not_delete_a_database_on_retry(self):
        self.fixture.put(self.homes[1], 'chat', ['private'])
        with mock.patch.object(shared.gzip, 'open', side_effect=OSError('backup failed')):
            result = shared.synchronize(self.request)
        self.assertTrue(result['pending'])
        self.assertTrue((self.homes[1] / 'state_5.sqlite').is_file())
        self.assertFalse(list((self.fixture.state / 'before-shared').glob('*/*.gz')))
        self.assertTrue(self.run_shared()['pending'])
        self.assertTrue((self.homes[1] / 'state_5.sqlite').is_file())

    def test_configuration_comments_provider_and_nested_sqlite_home_are_preserved(self):
        path = self.homes[1] / 'config.toml'
        content = '# keep this comment\n"sqlite_home" = "old"\nmodel_provider = "private"\n[model_providers.private]\nbase_url = "https://example.invalid/v1"\nsqlite_home = "a provider field"\n'
        path.write_text(content, encoding='utf-8')
        old, new = shared.config_bytes(path, self.canonical)
        self.assertEqual(old.decode().replace('\r\n', '\n'), content)
        self.assertIn('# keep this comment', new.decode())
        parsed = shared.tomllib.loads(new.decode())
        self.assertEqual(parsed['sqlite_home'], str(self.canonical))
        self.assertEqual(parsed['model_providers']['private']['sqlite_home'], 'a provider field')

    def test_previous_replication_entrypoint_cannot_recreate_private_copies(self):
        self.fixture.put(self.canonical, 'chat', ['first'])
        self.run_shared()
        result = fixture.sync.synchronize(self.homes, self.fixture.state)
        self.assertTrue(result['skipped'])
        self.assertEqual(result['storage'], 'shared')
        for home in self.homes[1:]:
            self.assertFalse((home / 'state_5.sqlite').exists())

    def test_migration_preserves_old_lock_directory_and_shares_writer_lock(self):
        locks = self.homes[1] / 'thread-writer-locks'
        locks.mkdir()
        (locks / 'old.lock').write_bytes(b'')
        self.run_shared()
        self.assertTrue(shared.exact_link(locks, self.canonical / 'thread-writer-locks'))
        self.assertEqual(len(list(self.homes[1].glob('.history-locks-before-shared-*/old.lock'))), 1)
        (locks / 'same.lock').write_bytes(b'')
        self.assertTrue(os.path.samefile(locks / 'same.lock', self.homes[2] / 'thread-writer-locks/same.lock'))


class AppServer:
    def __init__(self, binary, home):
        env = {k: v for k, v in os.environ.items() if not k.startswith(('CODEX', 'OPENAI_'))}
        env['CODEX_HOME'] = str(home)
        self.process = subprocess.Popen([str(binary), 'app-server', '--listen', 'stdio://'],
                                        cwd=home, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                        stderr=subprocess.DEVNULL, text=True, encoding='utf-8',
                                        creationflags=0x08000000 if os.name == 'nt' else 0)
        self.responses, self.next_id = queue.Queue(), 0
        def read():
            for line in self.process.stdout:
                try:
                    self.responses.put(json.loads(line))
                except ValueError:
                    pass
        self.reader = threading.Thread(target=read, daemon=True)
        self.reader.start()
        self.rpc('initialize', {'clientInfo': {'name': 'cockpit-shared-storage-test', 'version': '1.0'}})
        self.process.stdin.write(json.dumps({'method': 'initialized'}) + '\n')
        self.process.stdin.flush()

    def rpc(self, method, params):
        self.next_id += 1
        identifier = self.next_id
        self.process.stdin.write(json.dumps({'id': identifier, 'method': method, 'params': params}) + '\n')
        self.process.stdin.flush()
        deadline = time.monotonic() + 20
        while time.monotonic() < deadline:
            response = self.responses.get(timeout=max(0.01, deadline - time.monotonic()))
            if response.get('id') != identifier:
                continue
            if 'error' in response:
                raise AssertionError(str(response['error']))
            return response['result']
        raise TimeoutError(method)

    def close(self):
        self.process.terminate()
        try:
            self.process.wait(timeout=8)
        except subprocess.TimeoutExpired:
            self.process.kill(); self.process.wait(timeout=8)
        self.reader.join(timeout=2)
        self.process.stdin.close(); self.process.stdout.close()


@unittest.skipUnless(os.environ.get('AGENTCOCKPIT_SHARED_SQLITE_SMOKE') == '1', 'opt-in installed Codex integration')
class RealSQLiteSharing(unittest.TestCase):
    def test_two_independent_codex_homes_read_the_same_live_sqlite_database(self):
        import tempfile
        binaries = list((Path.home() / 'AppData/Local/OpenAI/Codex/bin').glob('*/codex.exe'))
        if not binaries:
            self.skipTest('Installed Codex executable not found')
        binary = max(binaries, key=lambda p: p.stat().st_mtime_ns)
        with tempfile.TemporaryDirectory(prefix='cockpit-shared-app-server-') as directory:
            base = Path(directory).resolve(); canonical, private = base / 'canonical', base / 'private'
            canonical.mkdir(); private.mkdir()
            for home in (canonical, private):
                (home / 'config.toml').write_text('sqlite_home = ' + json.dumps(str(canonical)) + '\ncli_auth_credentials_store = "file"\n', encoding='utf-8')
            a = AppServer(binary, canonical)
            self.addCleanup(a.close)
            try:
                a.rpc('thread/list', {'limit': 100})
                db = canonical / 'state_5.sqlite'
                self.assertTrue(db.is_file())
                thread = str(uuid.uuid4())
                (canonical / 'sessions').mkdir(exist_ok=True)
                rollout = canonical / 'sessions' / ('rollout-2026-10-28T09-00-00-' + thread + '.jsonl')
                rollout.write_text(json.dumps({'timestamp': '2026-10-28T01:00:00.000Z', 'ordinal': 0,
                    'type': 'session_meta', 'payload': {'id': thread, 'timestamp': '2026-10-28T01:00:00.000Z',
                        'cwd': str(base), 'source': 'vscode', 'originator': 'Codex Desktop',
                        'cli_version': '0.164.0', 'model_provider': 'openai', 'base_instructions': {'text': 'synthetic fixture'}}}) + '\n', encoding='utf-8')
                values = {'id': thread, 'rollout_path': str(rollout), 'created_at': int(time.time()), 'updated_at': int(time.time()),
                          'source': 'vscode', 'model_provider': 'openai', 'cwd': str(base), 'title': 'shared fixture',
                          'name': 'shared fixture', 'sandbox_policy': '{"type":"read-only"}', 'approval_mode': 'never',
                          'has_user_event': 1, 'thread_source': 'user', 'recency_at': int(time.time()),
                          'originator': 'Codex Desktop', 'first_user_message': 'shared fixture', 'preview': 'shared fixture'}
                with closing(sqlite3.connect(db)) as con, con:
                    cols = [r[1] for r in con.execute('PRAGMA table_info(threads)')]
                    kept = [k for k in values if k in cols]
                    con.execute('INSERT INTO threads(' + ','.join(kept) + ') VALUES(' + ','.join('?' for _ in kept) + ')', [values[k] for k in kept])
                b = AppServer(binary, private)
                try:
                    for index, client in enumerate((a, b)):
                        response = client.rpc('thread/list', {'limit': 100, 'modelProviders': [], 'sourceKinds': ['cli', 'vscode', 'appServer']})
                        config = client.rpc('config/read', {})
                        self.assertIn(thread, [v['id'] for v in response['data']],
                                      json.dumps({'client': index, 'response': response, 'sqlite_home': config.get('config', {}).get('sqlite_home')}))
                    self.assertFalse((private / 'state_5.sqlite').exists())
                    with closing(sqlite3.connect(db)) as con, con:
                        con.execute('UPDATE threads SET title=?,name=? WHERE id=?', ('revised', 'revised', thread))
                    for client in (a, b):
                        response = client.rpc('thread/list', {'limit': 100, 'modelProviders': [], 'sourceKinds': ['cli', 'vscode', 'appServer']})
                        row = next(v for v in response['data'] if v['id'] == thread)
                        self.assertEqual(row.get('name') or row.get('preview'), 'revised')
                finally:
                    b.close()
            finally:
                a.close()
                self._cleanups.clear()


if __name__ == '__main__':
    unittest.main()
