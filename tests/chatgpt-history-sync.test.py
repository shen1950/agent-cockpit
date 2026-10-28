from contextlib import contextmanager
import importlib.util
import json
import os
from pathlib import Path
import sqlite3
import tempfile
import unittest
from unittest import mock


spec = importlib.util.spec_from_file_location('history_sync', Path(__file__).resolve().parents[1] / 'src-tauri/scripts/chatgpt-history-sync.py')
sync = importlib.util.module_from_spec(spec)
spec.loader.exec_module(sync)


@contextmanager
def database(path):
    con = sqlite3.connect(path)
    try:
        with con:
            yield con
    finally:
        con.close()


class HistoryReplication(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='cockpit-history-sync-')
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name).resolve()
        self.homes = [self.base / name for name in ('first', 'second', 'third')]
        self.state = self.base / 'sync'
        for root in self.homes:
            root.mkdir()
            (root / 'auth.json').write_bytes(b'private-' + root.name.encode())
            (root / 'config.toml').write_bytes(b'provider="private"')
            with database(root / 'state_5.sqlite') as con:
                con.executescript('''CREATE TABLE threads(id TEXT PRIMARY KEY,rollout_path TEXT NOT NULL,
                    title TEXT, name TEXT, created_at INTEGER,updated_at INTEGER,model_provider TEXT,
                    cwd TEXT, archived INTEGER DEFAULT 0,project_id TEXT,thread_section_id TEXT);
                    CREATE TABLE projects(id TEXT PRIMARY KEY,name TEXT);
                    CREATE TABLE project_roots(project_id TEXT,position INTEGER,path TEXT,PRIMARY KEY(project_id,position));
                    CREATE TABLE automations(id TEXT PRIMARY KEY,prompt TEXT);''')
            with database(root / 'thread_history_1.sqlite') as con:
                con.executescript('''CREATE TABLE thread_items(thread_id TEXT,item_id TEXT,item_json TEXT,PRIMARY KEY(thread_id,item_id));
                    CREATE TABLE thread_history_projection_state(thread_id TEXT PRIMARY KEY,next_rollout_byte_offset INTEGER);''')
            (root / 'sqlite').mkdir()
            with database(root / 'sqlite/codex-dev.db') as con:
                con.executescript('''CREATE TABLE local_thread_catalog(host_id TEXT,thread_id TEXT,display_title TEXT,
                    source_created_at REAL,source_updated_at REAL,source_kind TEXT,observation_sequence INTEGER,
                    PRIMARY KEY(host_id,thread_id));
                    CREATE TABLE local_thread_catalog_hosts(host_id TEXT PRIMARY KEY,host_kind TEXT);
                    INSERT INTO local_thread_catalog_hosts VALUES('local','local');
                    CREATE TABLE local_thread_catalog_metadata(id INTEGER PRIMARY KEY,catalog_revision INTEGER);
                    INSERT INTO local_thread_catalog_metadata VALUES(1,0);''')

    def put(self, root, thread, messages, updated=1, archived=0):
        relative = Path('archived_sessions' if archived else 'sessions') / (thread + '.jsonl')
        path = root / relative
        path.parent.mkdir(exist_ok=True)
        data = b''.join(json.dumps({'text': m}).encode() + b'\n' for m in messages)
        path.write_bytes(data)
        with database(root / 'state_5.sqlite') as con:
            con.execute('INSERT OR REPLACE INTO threads VALUES(?,?,?,?,?,?,?,?,?,?,?)',
                        (thread, str(path), thread, thread, 1, updated, 'openai', str(self.base), archived, None, None))
        with database(root / 'thread_history_1.sqlite') as con:
            con.execute('INSERT OR REPLACE INTO thread_items VALUES(?,?,?)', (thread, 'last', messages[-1]))
            con.execute('INSERT OR REPLACE INTO thread_history_projection_state VALUES(?,?)', (thread, len(data)))
        return path

    def run_sync(self):
        result = sync.synchronize(self.homes, self.state)
        for root in self.homes:
            self.assertEqual((root / 'auth.json').read_bytes(), b'private-' + root.name.encode())
            self.assertEqual((root / 'config.toml').read_bytes(), b'provider="private"')
        return result

    def contents(self, root, thread):
        with database(root / 'state_5.sqlite') as con:
            path = con.execute('SELECT rollout_path FROM threads WHERE id=?', (thread,)).fetchone()[0]
        return Path(path).read_bytes()

    def test_bidirectional_union_projection_and_catalog(self):
        self.put(self.homes[0], 'one', ['first'])
        self.put(self.homes[1], 'two', ['second'])
        result = self.run_sync()
        self.assertFalse(result['deferred'])
        for root in self.homes:
            self.assertIn(b'first', self.contents(root, 'one'))
            self.assertIn(b'second', self.contents(root, 'two'))
            with database(root / 'thread_history_1.sqlite') as con:
                self.assertEqual(con.execute('SELECT COUNT(*) FROM thread_items').fetchone()[0], 2)
            with database(root / 'sqlite/codex-dev.db') as con:
                self.assertEqual(con.execute('SELECT COUNT(*) FROM local_thread_catalog').fetchone()[0], 2)
        again = self.run_sync()
        self.assertEqual(again['rollouts'], 0)
        self.assertEqual(again['threads'], 0)
        self.assertEqual(again['catalog'], 0)

    def test_content_extension_wins_over_recent_stale_database_timestamp(self):
        self.put(self.homes[0], 'chat', ['first', 'yesterday evening'], updated=2)
        stale = self.put(self.homes[1], 'chat', ['first'], updated=999999)
        original = stale.read_bytes()
        result = self.run_sync()
        self.assertFalse(result['conflicts'])
        self.assertIn(b'yesterday evening', self.contents(self.homes[1], 'chat'))
        backups = list((self.state / 'backups-v1').glob('*/sessions/chat.jsonl'))
        self.assertEqual(len(backups), 1)
        self.assertEqual(backups[0].read_bytes(), original)

    def test_opening_a_stale_chat_only_adds_settings_and_does_not_block_recovery(self):
        self.put(self.homes[0], 'chat', ['first', 'evening continuation'])
        stale = self.put(self.homes[1], 'chat', ['first'], updated=99999)
        with stale.open('ab') as file:
            # Deliberately larger than the real new conversation message.
            file.write(json.dumps({'type': 'event_msg', 'payload': {
                'type': 'thread_settings_applied', 'settings': 'x' * 10000}}).encode() + b'\n')
        before = stale.read_bytes()
        result = self.run_sync()
        self.assertFalse(result['conflicts'])
        self.assertFalse(result['deferred'])
        self.assertIn(b'evening continuation', self.contents(self.homes[1], 'chat'))
        backups = list((self.state / 'backups-v1').glob('*/sessions/chat.jsonl'))
        self.assertEqual(backups[0].read_bytes(), before)
        with database(self.homes[1] / 'thread_history_1.sqlite') as con:
            self.assertEqual(con.execute('SELECT item_json FROM thread_items').fetchone()[0], 'evening continuation')

    def test_stale_derived_projection_is_reset_and_rebuilt_from_complete_rollout(self):
        self.put(self.homes[0], 'chat', ['first'])
        for root in self.homes:
            with database(root / 'state_5.sqlite') as con:
                con.execute("ALTER TABLE threads ADD COLUMN history_mode TEXT DEFAULT 'paginated'")
        with database(self.homes[0] / 'thread_history_1.sqlite') as con:
            con.execute('UPDATE thread_history_projection_state SET next_rollout_byte_offset=999999')
        result = self.run_sync()
        self.assertFalse(result['deferred'])
        for root in self.homes:
            with database(root / 'state_5.sqlite') as con:
                self.assertEqual(con.execute('SELECT history_mode FROM threads').fetchone()[0], 'legacy')
            with database(root / 'thread_history_1.sqlite') as con:
                self.assertEqual(con.execute('SELECT COUNT(*) FROM thread_history_projection_state').fetchone()[0], 0)

    def test_later_continuation_in_a_different_window_propagates_back(self):
        self.put(self.homes[0], 'chat', ['first'])
        self.run_sync()
        self.put(self.homes[2], 'chat', ['first', 'continued elsewhere'], updated=20)
        self.run_sync()
        for root in self.homes:
            self.assertIn(b'continued elsewhere', self.contents(root, 'chat'))

    def test_divergent_continuations_are_never_overwritten(self):
        a = self.put(self.homes[0], 'chat', ['first', 'branch A'])
        b = self.put(self.homes[1], 'chat', ['first', 'longer branch B'])
        before = (a.read_bytes(), b.read_bytes())
        result = self.run_sync()
        self.assertEqual(result['conflicts'][0]['thread'], 'chat')
        self.assertEqual((a.read_bytes(), b.read_bytes()), before)

    def test_active_partial_destination_record_is_deferred(self):
        self.put(self.homes[0], 'chat', ['first', 'complete'])
        b = self.put(self.homes[1], 'chat', ['first'])
        with b.open('ab') as file:
            file.write(b'{"text":"in-progress')
        before = b.read_bytes()
        result = self.run_sync()
        self.assertTrue(result['deferred'])
        self.assertEqual(b.read_bytes(), before)

    def test_wal_commits_are_seen_and_background_jobs_are_not_copied(self):
        db = self.homes[0] / 'state_5.sqlite'
        with database(db) as con:
            con.execute('PRAGMA journal_mode=WAL')
            con.execute("INSERT INTO automations VALUES('job','do not replicate')")
            con.commit()
            self.put(self.homes[0], 'chat', ['first'])
            self.run_sync()
        for root in self.homes[1:]:
            self.assertIn(b'first', self.contents(root, 'chat'))
            with database(root / 'state_5.sqlite') as con:
                self.assertEqual(con.execute('SELECT COUNT(*) FROM automations').fetchone()[0], 0)

    def test_failed_projection_is_retried_after_thread_row_was_committed(self):
        self.put(self.homes[0], 'chat', ['first'])
        with mock.patch.object(sync.Replicator, 'copy_projection', side_effect=sqlite3.OperationalError('busy')):
            self.assertTrue(self.run_sync()['deferred'])
        result = self.run_sync()
        self.assertFalse(result['deferred'])
        with database(self.homes[1] / 'thread_history_1.sqlite') as con:
            self.assertEqual(con.execute('SELECT COUNT(*) FROM thread_items').fetchone()[0], 1)

    def test_external_rollout_path_cannot_copy_credentials(self):
        self.put(self.homes[0], 'chat', ['first'])
        with database(self.homes[0] / 'state_5.sqlite') as con:
            con.execute('UPDATE threads SET rollout_path=?', (str(self.homes[0] / 'auth.json'),))
        result = self.run_sync()
        self.assertTrue(result['deferred'])
        self.assertFalse((self.homes[1] / 'sessions/chat.jsonl').exists())

    def test_concurrent_sync_worker_cannot_take_the_same_lock(self):
        with sync.lock_file(self.state / 'sync.lock'):
            with self.assertRaises(OSError):
                self.run_sync()

    def test_archive_status_is_preserved(self):
        self.put(self.homes[0], 'chat', ['first'], archived=1)
        self.run_sync()
        for root in self.homes[1:]:
            with database(root / 'state_5.sqlite') as con:
                self.assertEqual(con.execute('SELECT archived FROM threads').fetchone()[0], 1)
            with database(root / 'sqlite/codex-dev.db') as con:
                self.assertEqual(con.execute('SELECT COUNT(*) FROM local_thread_catalog').fetchone()[0], 0)

    def test_background_children_and_guardian_reviews_never_enter_the_sidebar(self):
        self.put(self.homes[0], 'real-chat', ['user request'])
        self.put(self.homes[0], 'child', ['background work'])
        self.put(self.homes[0], 'review', ['guardian review'])
        self.put(self.homes[0], 'old-child', ['legacy child'])
        for root in self.homes:
            with database(root / 'state_5.sqlite') as con:
                con.execute('ALTER TABLE threads ADD COLUMN thread_source TEXT')
                con.execute('ALTER TABLE threads ADD COLUMN source TEXT')
                con.execute("UPDATE threads SET thread_source='subagent' WHERE id='child'")
                con.execute("UPDATE threads SET thread_source='guardian_review' WHERE id='review'")
                con.execute('UPDATE threads SET source=? WHERE id=?', (json.dumps({'subagent': {'other': 'guardian'}}), 'old-child'))
        result = self.run_sync()
        self.assertFalse(result['deferred'])
        for root in self.homes:
            with database(root / 'sqlite/codex-dev.db') as con:
                self.assertEqual(con.execute('SELECT thread_id FROM local_thread_catalog').fetchall(), [('real-chat',)])
            # Internal history is still available to its parent.
            self.assertIn(b'background work', self.contents(root, 'child'))
        # Clean a catalog polluted by a previous version even when content and
        # completion fingerprints are unchanged on the next pass.
        with database(self.homes[1] / 'sqlite/codex-dev.db') as con:
            con.execute("INSERT INTO local_thread_catalog(host_id,thread_id,display_title) VALUES('local','child','raw-id')")
        self.run_sync()
        with database(self.homes[1] / 'sqlite/codex-dev.db') as con:
            self.assertEqual(con.execute('SELECT thread_id FROM local_thread_catalog').fetchall(), [('real-chat',)])


if __name__ == '__main__':
    unittest.main()
