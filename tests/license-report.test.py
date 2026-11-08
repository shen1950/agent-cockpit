"""Run via agsync build --test-cmd. No network or user data is used."""
import importlib.util
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('license_report', Path(__file__).resolve().parents[1] / 'tools/license_report.py')
report = importlib.util.module_from_spec(spec)
spec.loader.exec_module(report)


class LockedLicenseTests(unittest.TestCase):
    def test_missing_version_is_unknown_even_when_another_version_is_cached(self):
        with tempfile.TemporaryDirectory() as tmp:
            host = Path(tmp)
            other = host / 'example-2.0.0'
            other.mkdir()
            (other / 'Cargo.toml').write_text('license = "MIT"\n', encoding='utf-8')
            self.assertEqual(report.read_license('example', '1.0.0', [host]), 'UNKNOWN')
            self.assertEqual(report.read_license('example', '2.0.0', [host]), 'MIT')

    def test_license_file_requires_manual_reading(self):
        with tempfile.TemporaryDirectory() as tmp:
            host = Path(tmp)
            exact = host / 'example-1.0.0'
            exact.mkdir()
            (exact / 'Cargo.toml').write_text('license-file = "COPYING"\n', encoding='utf-8')
            self.assertEqual(report.read_license('example', '1.0.0', [host]), 'LICENSE-FILE')


if __name__ == '__main__':
    unittest.main()
