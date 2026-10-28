"""History-copy integration tests. Run through the registered agsync check.

AGENTCOCKPIT_HISTORY_SMOKE=1 additionally retains a read-only native-history
snapshot in the system temp directory, without printing conversation contents.
"""
from contextlib import closing
import gzip
import importlib.util
import json
import os
from pathlib import Path
import sqlite3
import stat
import tempfile
from types import SimpleNamespace
import unittest
from unittest import mock

SCRIPT = Path(__file__).resolve().parents[1] / 'src-tauri/scripts/chatgpt-history.py'
SPEC = importlib.util.spec_from_file_location('chatgpt_history_copy', SCRIPT)
history = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(history)


class HistoryCopyTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='agentcockpit-history-unit-')
        self.addCleanup(self.temp.cleanup)
        root = Path(self.temp.name).resolve()
        self.source = root / 'source # % 中文'
        self.target = root / 'private account'
        self.source.mkdir()
        self.target.mkdir()
        self.target_auth = b'{"tokens":{"access_token":"target-auth-must-stay"}}\r\n'
        self.target_config = b'model_provider = "private-provider"\r\n'
        (self.target / 'auth.json').write_bytes(self.target_auth)
        (self.target / 'config.toml').write_bytes(self.target_config)

    def assert_credentials_unchanged(self):
        self.assertEqual((self.target / 'auth.json').read_bytes(), self.target_auth)
        self.assertEqual((self.target / 'config.toml').read_bytes(), self.target_config)

    def populate(self, wal=False):
        self.rollout = self.source / 'sessions/2026/10/rollout-one.jsonl'
        self.rollout.parent.mkdir(parents=True)
        raw = (json.dumps({'type': 'session_meta', 'cwd': str(self.source), 'text': '原文'},
                          ensure_ascii=False) + '\r\n{"type":"message","text":"unchanged"}\r\n').encode('utf-8')
        self.rollout.write_bytes(raw)
        (self.rollout.parent / 'archived.jsonl.gz').write_bytes(gzip.compress(raw))
        try:
            from compression import zstd
        except ImportError:
            import zstandard as zstd
        (self.rollout.parent / 'compressed.jsonl.zst').write_bytes(zstd.compress(raw))
        db = self.source / 'state_5.sqlite'
        con = sqlite3.connect(db)
        con.executescript('''
            CREATE TABLE threads(id TEXT PRIMARY KEY,rollout_path TEXT,host_id TEXT,title TEXT);
            CREATE TABLE projects(id TEXT PRIMARY KEY,path TEXT,host_id TEXT);
        ''')
        if wal:
            con.execute('PRAGMA journal_mode=WAL')
            con.execute('PRAGMA wal_autocheckpoint=0')
            con.execute('PRAGMA wal_checkpoint(TRUNCATE)')
        con.execute('INSERT INTO threads VALUES(?,?,?,?)',
                    ('one', str(self.rollout), 'local:' + str(self.source), "A title with 'quotes'"))
        con.execute('INSERT INTO projects VALUES(?,?,?)',
                    ('project', str(self.source / 'worktrees/project'), 'local:' + str(self.source)))
        (self.source / 'worktrees/project').mkdir(parents=True)
        con.commit()
        if wal:
            self.addCleanup(con.close)
        else:
            con.close()
        catalog = self.source / 'sqlite/catalog/thread_catalog_1.sqlite'
        catalog.parent.mkdir(parents=True)
        with closing(sqlite3.connect(catalog)) as index:
            index.execute('CREATE TABLE entries(local_host_id TEXT,rollout_file_path TEXT)')
            index.execute('INSERT INTO entries VALUES(?,?)', ('local:' + str(self.source), str(self.rollout)))
            index.commit()
        state = {
            'local-projects': {'project': {'path': str(self.source / 'worktrees/project'), 'api_key': 'DO-NOT-COPY'}},
            'thread-project-membership-host-ids': {'one': 'local:' + str(self.source)},
            'app-server-projects-migration-by-host': {'local:' + str(self.source): True},
            'project-api-key': 'DO-NOT-COPY',
            'electron-local-remote-control-installation-id': 'DO-NOT-COPY',
            'electron-persisted-atom-state': {
                'sidebar-custom-sections-v3': {'items': ['project']},
                'plugin-oauth-install-reconciliation-by-state-v1': 'DO-NOT-COPY',
            },
        }
        (self.source / history.STATE_FILE).write_text(json.dumps(state), encoding='utf-8')
        (self.source / 'session_index.jsonl').write_text(json.dumps({
            'id': 'one', 'host_id': 'local:' + str(self.source), 'rollout_path': str(self.rollout),
        }) + '\n', encoding='utf-8')
        (self.source / 'auth.json').write_bytes(b'{"access_token":"SOURCE-AUTH-SECRET"}')
        (self.source / 'config.toml').write_bytes(b'api_key="SOURCE-CONFIG-SECRET"')
        (self.source / 'browser/GPUCache').mkdir(parents=True)
        (self.source / 'browser/GPUCache/secret').write_text('SOURCE-CACHE-SECRET', encoding='utf-8')
        return db

    def test_wal_snapshot_rollout_bytes_paths_hosts_and_credentials(self):
        db = self.populate(wal=True)
        self.assertGreater(Path(str(db) + '-wal').stat().st_size, 32)
        with closing(sqlite3.connect(db.as_uri() + '?immutable=1', uri=True)) as main_only:
            self.assertEqual(main_only.execute('SELECT COUNT(*) FROM threads').fetchone()[0], 0)
        # SQLite read-only connections can update WAL shared-memory read marks;
        # the database, committed WAL, credentials and rollout bytes stay intact.
        before = {p.relative_to(self.source): p.read_bytes() for p in self.source.rglob('*') if p.is_file() and not p.name.endswith('-shm')}
        result = history.copy_history(self.source, self.target)
        self.assertEqual((result['threads'], result['projects'], result['rollouts']), (1, 1, 3))
        self.assertFalse(result['preserved_existing'])
        self.assertEqual(result['restored_compressed'], 2)
        self.assertEqual((self.target / self.rollout.relative_to(self.source)).with_name('compressed.jsonl').read_bytes(), self.rollout.read_bytes())
        copied_rollout = self.target / self.rollout.relative_to(self.source)
        with closing(sqlite3.connect(self.target / 'state_5.sqlite')) as copied:
            self.assertEqual(copied.execute('SELECT rollout_path,host_id FROM threads').fetchone(),
                             (str(copied_rollout), 'local:' + str(self.target)))
            self.assertEqual(copied.execute('PRAGMA quick_check').fetchone(), ('ok',))
            workspace = copied.execute('SELECT path FROM projects').fetchone()[0]
            self.assertEqual(workspace, str(self.source / 'worktrees/project'))
            self.assertTrue(Path(workspace).is_dir())
        with closing(sqlite3.connect(self.target / 'sqlite/catalog/thread_catalog_1.sqlite')) as copied:
            self.assertEqual(copied.execute('SELECT * FROM entries').fetchone(),
                             ('local:' + str(self.target), str(copied_rollout)))
        for relative, original in before.items():
            self.assertEqual((self.source / relative).read_bytes(), original, str(relative))
            if relative.parts[0] in history.ROLLOUT_DIRS and relative.name.lower().endswith(history.ROLLOUT_SUFFIXES):
                self.assertEqual((self.target / relative).read_bytes(), original)
        state = json.loads((self.target / history.STATE_FILE).read_text(encoding='utf-8'))
        self.assertEqual(state['local-projects']['project']['path'], str(self.source / 'worktrees/project'))
        self.assertEqual(state['thread-project-membership-host-ids']['one'], 'local:' + str(self.target))
        self.assertIn('local:' + str(self.target), state['app-server-projects-migration-by-host'])
        self.assertNotIn('DO-NOT-COPY', json.dumps(state))
        self.assertFalse((self.target / 'browser').exists())
        with closing(history.readonly(db)) as original:
            self.assertEqual(original.execute('SELECT COUNT(*) FROM threads').fetchone()[0], 1)
        self.assert_credentials_unchanged()

    def test_existing_nested_history_is_preserved(self):
        self.populate()
        existing = self.target / 'sqlite/legacy/thread_catalog_1.sqlite'
        existing.parent.mkdir(parents=True)
        with closing(sqlite3.connect(existing)) as con:
            con.execute('CREATE TABLE threads(id TEXT PRIMARY KEY)')
            con.execute('INSERT INTO threads VALUES(?)', ('already-here',))
            con.commit()
        before = existing.read_bytes()
        result = history.copy_history(self.source, self.target)
        self.assertTrue(result['preserved_existing'])
        self.assertEqual(existing.read_bytes(), before)
        self.assertFalse((self.target / 'state_5.sqlite').exists())
        self.assert_credentials_unchanged()

    def test_existing_index_alone_prevents_replacement(self):
        self.populate()
        existing = self.target / 'session_index.jsonl'
        existing.write_bytes(b'{"id":"target-only"}\r\n')
        result = history.copy_history(self.source, self.target)
        self.assertTrue(result['preserved_existing'])
        self.assertEqual(existing.read_bytes(), b'{"id":"target-only"}\r\n')
        self.assertFalse((self.target / 'sessions').exists())

    def test_existing_turn_projection_is_preserved_without_threads_table(self):
        self.populate()
        database = self.target / 'thread_history_1.sqlite'
        with closing(sqlite3.connect(database)) as con:
            con.executescript("CREATE TABLE thread_turns(id TEXT); INSERT INTO thread_turns VALUES('kept');")
        before = database.read_bytes()
        result = history.copy_history(self.source, self.target)
        self.assertTrue(result['preserved_existing'])
        self.assertEqual(database.read_bytes(), before)
        self.assertFalse((self.target / 'state_5.sqlite').exists())

    def test_archived_dangling_index_is_kept_using_current_root_status(self):
        db = self.populate()
        with closing(sqlite3.connect(db)) as con:
            con.execute('ALTER TABLE threads ADD COLUMN archived INTEGER DEFAULT 0')
            con.execute('INSERT INTO threads VALUES(?,?,?,?,?)', ('archived', str(self.source / 'archived_sessions/missing.jsonl'), 'local:' + str(self.source), 'old', 1))
            con.commit()
        old = self.source / 'sqlite/state_5.sqlite'
        with closing(sqlite3.connect(old)) as con:
            con.executescript('CREATE TABLE threads(id TEXT,rollout_path TEXT,archived INTEGER)')
            con.execute('INSERT INTO threads VALUES(?,?,?)', ('archived', str(self.source / 'archived_sessions/missing.jsonl'), 0))
            con.commit()
        result = history.copy_history(self.source, self.target)
        self.assertEqual(result['missing_archived_rollouts'], 1)
        with closing(sqlite3.connect(self.target / 'state_5.sqlite')) as con:
            self.assertEqual(con.execute("SELECT rollout_path FROM threads WHERE id='archived'").fetchone()[0], str(self.target / 'archived_sessions/missing.jsonl'))
        self.assertFalse((self.target / 'archived_sessions/missing.jsonl').exists())

    def test_compressed_rollout_is_restored_and_extended_windows_prefix_is_recognized(self):
        self.populate()
        self.rollout.unlink()
        self.rollout.with_name('compressed.jsonl.zst').rename(Path(str(self.rollout) + '.zst'))
        history.copy_history(self.source, self.target)
        with closing(sqlite3.connect(self.target / 'state_5.sqlite')) as con:
            self.assertTrue(Path(con.execute('SELECT rollout_path FROM threads').fetchone()[0]).is_file())
        self.assertEqual(history.relative_home('\\\\?\\C:\\Users\\test\\.codex\\sessions\\one.jsonl', Path('C:/Users/test/.codex')), Path('sessions/one.jsonl'))

    def test_empty_cli_initialization_is_replaced_and_ui_state_is_kept(self):
        self.populate()
        with closing(sqlite3.connect(self.target / 'state_5.sqlite')) as con:
            con.executescript('CREATE TABLE threads(id TEXT); CREATE TABLE projects(id TEXT);')
        previous = {'electron-main-window-bounds': {'width': 640}, 'provider_settings': {'token': 'target-only'}}
        (self.target / history.STATE_FILE).write_text(json.dumps(previous), encoding='utf-8')
        result = history.copy_history(self.source, self.target)
        self.assertFalse(result['preserved_existing'])
        self.assertEqual(result['threads'], 1)
        state = json.loads((self.target / history.STATE_FILE).read_text(encoding='utf-8'))
        self.assertEqual(state['electron-main-window-bounds'], previous['electron-main-window-bounds'])
        self.assertEqual(state['provider_settings'], previous['provider_settings'])
        self.assertEqual(history.history_count(self.target), 1)
        self.assert_credentials_unchanged()

    def test_empty_initialization_is_restored_after_late_failure(self):
        self.populate()
        empty_db = self.target / 'state_5.sqlite'
        with closing(sqlite3.connect(empty_db)) as con:
            con.executescript('CREATE TABLE threads(id TEXT); CREATE TABLE projects(id TEXT);')
        empty_state = self.target / history.STATE_FILE
        empty_state.write_bytes(b'{"electron-main-window-bounds":{"width":640}}\r\n')
        original_db, original_state = empty_db.read_bytes(), empty_state.read_bytes()
        publish = history._publish_file

        def fail_marker(source, destination):
            if destination.name == history.MARKER_FILE:
                raise OSError('simulated empty-target failure')
            return publish(source, destination)

        with mock.patch.object(history, '_publish_file', side_effect=fail_marker):
            with self.assertRaisesRegex(OSError, 'empty-target failure'):
                history.copy_history(self.source, self.target)
        self.assertEqual(empty_db.read_bytes(), original_db)
        self.assertEqual(empty_state.read_bytes(), original_state)
        self.assertFalse((self.target / 'sessions').exists())
        self.assert_credentials_unchanged()

    def test_background_jobs_are_removed_only_from_copied_databases(self):
        source_db = self.populate()
        with closing(sqlite3.connect(source_db)) as con:
            con.executescript('CREATE TABLE agent_jobs(id TEXT,prompt TEXT); CREATE TABLE agent_job_items(id TEXT,job_id TEXT);')
            con.execute('INSERT INTO agent_jobs VALUES(?,?)', ('job', 'SOURCE-JOB-SECRET'))
            con.execute('INSERT INTO agent_job_items VALUES(?,?)', ('item', 'job'))
            con.commit()
        history.copy_history(self.source, self.target)
        with closing(history.readonly(source_db)) as con:
            self.assertEqual(con.execute('SELECT COUNT(*) FROM agent_jobs').fetchone()[0], 1)
        with closing(sqlite3.connect(self.target / 'state_5.sqlite')) as con:
            self.assertEqual(con.execute('SELECT COUNT(*) FROM agent_jobs').fetchone()[0], 0)
            self.assertEqual(con.execute('SELECT COUNT(*) FROM agent_job_items').fetchone()[0], 0)
            self.assertEqual(con.execute('SELECT COUNT(*) FROM threads').fetchone()[0], 1)
        self.assertNotIn(b'SOURCE-JOB-SECRET', (self.target / 'state_5.sqlite').read_bytes())

    def test_codex_dev_keeps_catalog_but_not_automations_or_account_cache(self):
        self.populate()
        sidecar = self.source / 'sqlite/codex-dev.db'
        with closing(sqlite3.connect(sidecar)) as con:
            con.executescript('''CREATE TABLE local_thread_catalog(host_id TEXT,thread_id TEXT,cwd TEXT);
                CREATE TABLE codex_schema_migrations(id TEXT);
                CREATE TABLE thread_timeline_ledger(host_id TEXT,payload_json TEXT);
                CREATE TABLE automations(id TEXT,prompt TEXT);
                CREATE TABLE account_auth_cache(token TEXT);''')
            con.execute('INSERT INTO local_thread_catalog VALUES(?,?,?)', ('local:' + str(self.source), 'one', str(self.source / 'worktrees/project')))
            con.execute('INSERT INTO codex_schema_migrations VALUES(?)', ('schema-one',))
            con.execute('INSERT INTO thread_timeline_ledger VALUES(?,?)',
                        ('local:' + str(self.source), json.dumps({'host_id': 'local:' + str(self.source)})))
            con.execute('INSERT INTO automations VALUES(?,?)', ('old-job', 'SOURCE-AUTOMATION-SECRET'))
            con.execute('INSERT INTO account_auth_cache VALUES(?)', ('SOURCE-ACCOUNT-SECRET',))
            con.commit()
        history.copy_history(self.source, self.target)
        copied = self.target / 'sqlite/codex-dev.db'
        with closing(sqlite3.connect(copied)) as con:
            self.assertEqual(con.execute('SELECT host_id FROM local_thread_catalog').fetchone()[0], 'local:' + str(self.target))
            self.assertEqual(con.execute('SELECT COUNT(*) FROM codex_schema_migrations').fetchone()[0], 1)
            self.assertEqual(con.execute('SELECT COUNT(*) FROM automations').fetchone()[0], 0)
            self.assertEqual(con.execute('SELECT COUNT(*) FROM account_auth_cache').fetchone()[0], 0)
        self.assertNotIn(b'SOURCE-AUTOMATION-SECRET', copied.read_bytes())
        self.assertNotIn(b'SOURCE-ACCOUNT-SECRET', copied.read_bytes())

    def test_primary_root_database_counts_take_precedence(self):
        self.populate()
        nested = self.source / 'sqlite/legacy/state_5.sqlite'
        nested.parent.mkdir(parents=True)
        with closing(sqlite3.connect(nested)) as con:
            con.execute('CREATE TABLE threads(id TEXT PRIMARY KEY)')
            con.executemany('INSERT INTO threads VALUES(?)', [('legacy-one',), ('legacy-two',)])
            con.commit()
        result = history.copy_history(self.source, self.target)
        self.assertEqual(result['threads'], 1)
        self.assertEqual(history.history_count(self.target), 1)

    def test_late_publish_failure_rolls_back_every_new_file(self):
        self.populate()
        publish = history._publish_file

        def fail_marker(source, destination):
            if destination.name == history.MARKER_FILE:
                raise OSError('simulated late publication failure')
            return publish(source, destination)

        with mock.patch.object(history, '_publish_file', side_effect=fail_marker):
            with self.assertRaisesRegex(OSError, 'simulated late'):
                history.copy_history(self.source, self.target)
        self.assertEqual({p.name for p in self.target.iterdir()}, {'auth.json', 'config.toml'})
        self.assert_credentials_unchanged()
        self.assertTrue(self.rollout.is_file())

    def test_new_target_history_during_snapshot_is_preserved(self):
        self.populate()
        original = history.history_state

        def concurrent_history(path, relocate):
            (self.target / 'session_index.jsonl').write_bytes(b'{"id":"new-target-thread"}\n')
            return original(path, relocate)

        with mock.patch.object(history, 'history_state', side_effect=concurrent_history):
            result = history.copy_history(self.source, self.target)
        self.assertTrue(result['preserved_existing'])
        self.assertEqual((self.target / 'session_index.jsonl').read_bytes(), b'{"id":"new-target-thread"}\n')
        self.assertFalse((self.target / 'state_5.sqlite').exists())
        self.assertFalse(list(self.target.glob('.history-stage-*')))

    def test_exclusive_publication_never_overwrites_a_last_second_file(self):
        self.populate()
        publish = history._publish_file

        def conflict(source, destination):
            if destination.name == 'state_5.sqlite':
                destination.write_bytes(b'a file created by another owner')
            return publish(source, destination)

        with mock.patch.object(history, '_publish_file', side_effect=conflict):
            with self.assertRaises(FileExistsError):
                history.copy_history(self.source, self.target)
        self.assertEqual((self.target / 'state_5.sqlite').read_bytes(), b'a file created by another owner')
        self.assert_credentials_unchanged()
        self.assertFalse((self.target / history.MARKER_FILE).exists())

    def test_missing_or_parent_traversing_rollout_aborts_before_publication(self):
        db = self.populate()
        with closing(sqlite3.connect(db)) as con:
            con.execute('UPDATE threads SET rollout_path=?', (str(self.source / 'sessions/../auth.json'),))
            con.commit()
        with self.assertRaisesRegex(ValueError, 'traverses'):
            history.copy_history(self.source, self.target)
        self.assertEqual({p.name for p in self.target.iterdir()}, {'auth.json', 'config.toml'})
        self.assert_credentials_unchanged()

    def test_directory_links_are_rejected(self):
        self.populate()
        linked = self.source / 'sessions/linked'
        try:
            linked.symlink_to(self.target, target_is_directory=True)
        except OSError as error:
            self.skipTest('This account cannot create a test directory link: ' + str(error))
        with self.assertRaisesRegex(ValueError, 'link'):
            history.copy_history(self.source, self.target)
        self.assert_credentials_unchanged()

    def test_reparse_flag_is_checked_without_python312_is_junction(self):
        info = SimpleNamespace(st_mode=stat.S_IFDIR, st_file_attributes=0x400)
        with mock.patch.object(Path, 'lstat', return_value=info):
            self.assertTrue(history.is_link(self.source))

    def test_overlapping_homes_are_rejected(self):
        child = self.source / 'child'
        child.mkdir()
        with self.assertRaisesRegex(ValueError, 'overlap'):
            history.copy_history(self.source, child)


class ProjectVisibilityTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='agentcockpit-project-visibility-')
        self.addCleanup(self.temp.cleanup)
        root = Path(self.temp.name).resolve()
        self.source, self.target = root / 'source', root / 'target'
        self.source.mkdir(); self.target.mkdir()
        self.current, self.old = root / 'workspace/Current', root / 'workspace/Old'
        self.current.mkdir(parents=True); self.old.mkdir()
        self.copied = history.time.time()
        self.stamp = int(self.copied * 1000)
        kept = {'id': 'local-current', 'name': 'Current', 'rootPaths': [str(self.current)], 'createdAt': 1, 'updatedAt': 1}
        restored = {'id': 'local-old', 'name': 'Old', 'rootPaths': [str(self.old)], 'createdAt': self.stamp + 1000, 'updatedAt': self.stamp + 1000}
        self.original = {'local-projects': {'local-current': kept}, 'electron-saved-workspace-roots': [str(self.current), str(self.old)],
                         'electron-completed-local-data-migration-ids': [history.LOCAL_PROJECT_MIGRATION], 'electron-local-data-migration-id': 3,
                         'project-order': ['local-current'], 'app-server-pending-project-deletions-by-host': {'local:host': ['deleted-before-copy']}}
        self.initial = {**self.original, 'local-projects': {'local-current': kept, 'local-old': restored},
                        'project-order': ['local-old', 'local-current'], 'electron-completed-local-data-migration-ids': [],
                        'app-server-project-id-by-legacy-project-id-by-host': {'local:target': {'local-old': 'db-old', 'local-current': 'db-current'}},
                        'electron-main-window-bounds': {'width': 1234}}
        (self.source / history.STATE_FILE).write_text(json.dumps(self.original), encoding='utf-8')
        (self.target / history.STATE_FILE).write_text(json.dumps(self.initial), encoding='utf-8')
        (self.target / history.MARKER_FILE).write_text(json.dumps({'source': str(self.source), 'copied_at': self.copied}), encoding='utf-8')
        self.auth, self.config = b'{"token":"keep-existing-auth"}', b'provider="unchanged"'
        (self.target / 'auth.json').write_bytes(self.auth); (self.target / 'config.toml').write_bytes(self.config)
        for home in (self.source, self.target):
            with closing(sqlite3.connect(home / 'state_5.sqlite')) as con:
                con.executescript('''CREATE TABLE projects(id TEXT PRIMARY KEY,name TEXT,metadata TEXT,position INTEGER,created_at_ms INTEGER,updated_at_ms INTEGER);
                    CREATE TABLE project_roots(project_id TEXT,position INTEGER,path TEXT);
                    CREATE TABLE project_idempotency_keys(key TEXT,project_id TEXT,created_at_ms INTEGER);
                    CREATE TABLE threads(id TEXT PRIMARY KEY,project_id TEXT,title TEXT);''')
                con.execute('INSERT INTO projects VALUES(?,?,?,?,?,?)', ('db-current', 'Current', '{}', 1, 1, 1))
                con.execute('INSERT INTO project_roots VALUES(?,?,?)', ('db-current', 0, str(self.current)))
                con.execute('INSERT INTO threads VALUES(?,?,?)', ('kept-chat', 'db-current', 'Kept conversation'))
                if home == self.target:
                    con.execute('INSERT INTO projects VALUES(?,?,?,?,?,?)', ('db-old', 'Old', '{}', 0, self.stamp + 2000, self.stamp + 2000))
                    con.execute('INSERT INTO project_roots VALUES(?,?,?)', ('db-old', 0, str(self.old)))
                    con.execute('INSERT INTO project_idempotency_keys VALUES(?,?,?)', ('recreated-old', 'db-old', self.stamp + 2000))
                con.commit()

    def test_copy_preserves_project_migrations_and_prunes_obsolete_workspace_suggestions(self):
        result = history.history_state(self.source / history.STATE_FILE, lambda x: x)
        self.assertEqual(result['electron-completed-local-data-migration-ids'], [history.LOCAL_PROJECT_MIGRATION])
        self.assertEqual(result['electron-local-data-migration-id'], 3)
        self.assertEqual(result['electron-saved-workspace-roots'], [str(self.current)])
        self.assertEqual(result['app-server-pending-project-deletions-by-host'], self.original['app-server-pending-project-deletions-by-host'])
        legacy = {**self.original, 'electron-completed-local-data-migration-ids': []}
        self.assertEqual(history.project_visibility_state(legacy)['electron-saved-workspace-roots'], [str(self.current), str(self.old)])

    def test_dry_run_identifies_recreated_empty_projects_without_changing_data(self):
        path = self.target / history.STATE_FILE
        before = path.read_bytes()
        result = history.repair_projects(self.source, self.target)
        self.assertEqual(result['legacy_ids'], ['local-old']); self.assertEqual(result['project_ids'], ['db-old'])
        self.assertEqual(path.read_bytes(), before)
        self.assertFalse((self.target / '.project-repair-backups').exists())

    def test_offline_repair_keeps_conversations_files_credentials_and_backup(self):
        before = (self.target / history.STATE_FILE).read_bytes()
        plan = history.project_repair_plan(self.source, self.target)
        result = history.repair_projects(self.source, self.target, False, plan)
        self.assertTrue(result['applied']); self.assertTrue(self.old.is_dir())
        state = json.loads((self.target / history.STATE_FILE).read_text(encoding='utf-8'))
        self.assertNotIn('local-old', state['local-projects'])
        self.assertEqual(state['project-order'], ['local-current'])
        self.assertEqual(state['electron-saved-workspace-roots'], [str(self.current)])
        self.assertEqual(state['electron-main-window-bounds'], {'width': 1234})
        self.assertEqual((Path(result['backup']) / history.STATE_FILE).read_bytes(), before)
        with closing(sqlite3.connect(self.target / 'state_5.sqlite')) as con:
            self.assertEqual(con.execute('SELECT id FROM projects').fetchall(), [('db-current',)])
            self.assertEqual(con.execute('SELECT * FROM threads').fetchall(), [('kept-chat', 'db-current', 'Kept conversation')])
        self.assertEqual((self.target / 'auth.json').read_bytes(), self.auth)
        self.assertEqual((self.target / 'config.toml').read_bytes(), self.config)

    def test_user_edited_or_used_project_is_retained(self):
        state = dict(self.initial)
        state['local-projects']['local-old']['updatedAt'] += 5000
        (self.target / history.STATE_FILE).write_text(json.dumps(state), encoding='utf-8')
        plan = history.project_repair_plan(self.source, self.target)
        self.assertEqual(plan['legacy_ids'], []); self.assertEqual(plan['uncertain'], 1)
        state['local-projects']['local-old']['updatedAt'] -= 5000
        (self.target / history.STATE_FILE).write_text(json.dumps(state), encoding='utf-8')
        with closing(sqlite3.connect(self.target / 'state_5.sqlite')) as con:
            con.execute('INSERT INTO threads VALUES(?,?,?)', ('new-chat', 'db-old', 'Created after copy')); con.commit()
        self.assertEqual(history.project_repair_plan(self.source, self.target)['project_ids'], [])

    def test_queued_repair_stops_when_project_gains_a_conversation(self):
        plan = history.project_repair_plan(self.source, self.target)
        with closing(sqlite3.connect(self.target / 'state_5.sqlite')) as con:
            con.execute('INSERT INTO threads VALUES(?,?,?)', ('new-chat', 'db-old', 'Created after queue')); con.commit()
        with self.assertRaisesRegex(ValueError, 'changed'):
            history.repair_projects(self.source, self.target, False, plan)
        self.assertFalse((self.target / '.project-repair-backups').exists())

    def test_failed_state_publication_rolls_back_database_deletions(self):
        initial = (self.target / history.STATE_FILE).read_bytes()
        with mock.patch.object(history, '_atomic_bytes', side_effect=OSError('simulated locked state')):
            with self.assertRaisesRegex(OSError, 'locked state'):
                history.repair_projects(self.source, self.target, False)
        self.assertEqual((self.target / history.STATE_FILE).read_bytes(), initial)
        with closing(sqlite3.connect(self.target / 'state_5.sqlite')) as con:
            self.assertEqual(con.execute("SELECT COUNT(*) FROM projects WHERE id='db-old'").fetchone()[0], 1)


@unittest.skipUnless(os.environ.get('AGENTCOCKPIT_HISTORY_SMOKE') == '1', 'opt-in native snapshot')
class NativeHistorySnapshot(unittest.TestCase):
    def test_native_history_snapshot_keeps_credentials_unchanged(self):
        source = (Path.home() / '.codex').resolve()
        if not source.is_dir():
            self.skipTest('Native CODEX_HOME is absent')
        protected = {name: (source / name).read_bytes() if (source / name).is_file() else None
                     for name in ('auth.json', 'config.toml')}
        target = Path(tempfile.mkdtemp(prefix='agentcockpit-history-smoke-')).resolve()
        # Deliberately retained for review; it contains copied conversation data.
        result = history.copy_history(source, target)
        for name, original in protected.items():
            self.assertEqual((source / name).read_bytes() if (source / name).is_file() else None, original)
            self.assertFalse((target / name).exists())
        roots = sorted(target.glob('state_*.sqlite'))
        for db in roots:
            with closing(sqlite3.connect(db)) as con:
                if 'threads' in history.tables(con) and 'rollout_path' in history.columns(con, 'threads'):
                    available_columns = set(history.columns(con, 'threads'))
                    selection = 'rollout_path,archived' if 'archived' in available_columns else 'rollout_path,0'
                    for rollout, archived in con.execute('SELECT '+selection+' FROM threads WHERE rollout_path IS NOT NULL AND rollout_path<>\'\''):
                        self.assertTrue(Path(rollout).is_file() or archived)
                        self.assertTrue(Path(rollout).is_relative_to(target))
        print(json.dumps({'history_snapshot': str(target), 'threads': result.get('threads', 0),
                          'projects': result.get('projects', 0), 'rollouts': result.get('rollouts', 0),
                          'restored_compressed': result.get('restored_compressed', 0),
                          'missing_archived_rollouts': result.get('missing_archived_rollouts', 0)}))


if __name__ == '__main__':
    unittest.main()
