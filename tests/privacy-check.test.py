import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('privacy', Path(__file__).resolve().parents[1] / 'tools/privacy_check.py')
privacy = importlib.util.module_from_spec(spec)
spec.loader.exec_module(privacy)

class PrivacyGateTests(unittest.TestCase):
    def test_neutral_examples(self):
        self.assertEqual(privacy.scan_text('demo@example.test C:/Users/Example/app', 'fixture.txt'), [])

    def test_reports_location_without_values(self):
        sensitive = 'person' + '@' + 'mail-provider.org'
        findings = privacy.scan_text(sensitive, 'sample.txt')
        self.assertEqual(findings, [('sample.txt', 1, 'non-example-email')])
        self.assertNotIn(sensitive, repr(findings))

    def test_new_keys_not_allowed_with_known_fixtures(self):
        key = 'sk-' + 'X9sQ7pL2vR8mT4nB6hJ3zK5d'
        self.assertTrue(privacy.scan_text(key, 'ui/mock.js'))

    def test_private_terms_and_home_paths(self):
        self.assertTrue(privacy.scan_text('private-person', 'sample.txt', ['private-person']))
        personal_path = 'C:/Users/' + 'PrivatePerson/app'
        self.assertTrue(privacy.scan_text(personal_path, 'sample.txt'))

    def test_developer_install_roots_are_reported(self):
        # 拼出来，避免测试文件本身又成为“公开源码里的开发机路径”
        roots = ('Artificial' + '_Intelligence', 'Leno' + 'vo' + 'Softstore', 'npm' + '-global')
        for root in roots:
            self.assertTrue(
                privacy.scan_text('D:/' + root + '/Foo/Foo.exe', 'fixture.txt'), root
            )
        self.assertEqual(privacy.scan_text('D:/Apps/Foo/Foo.exe', 'fixture.txt'), [])

if __name__ == '__main__':
    unittest.main()
