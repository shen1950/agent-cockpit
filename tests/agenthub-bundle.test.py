import pathlib
import unittest
import zipfile

ROOT = pathlib.Path(__file__).resolve().parents[1]
VENDOR = ROOT / "src-tauri" / "vendor"


class AgenthubBundleTests(unittest.TestCase):
    def test_source_and_license_are_present(self):
        self.assertTrue((VENDOR / "agenthub.py").is_file())
        self.assertTrue((VENDOR / "console.html").is_file())
        self.assertIn("MIT License", (ROOT / "docs/third-party/AGENTHUB-LICENSE.txt").read_text(encoding="utf-8"))

    def test_libs_zip_has_crypto_cffi_and_path_file(self):
        with zipfile.ZipFile(VENDOR / "agenthub-libs.zip") as archive:
            names = set(archive.namelist())
        for required in (
            "Lib/site-packages/cryptography/__init__.py",
            "Lib/site-packages/cffi/__init__.py",
            "Lib/site-packages/pycparser/__init__.py",
            "python314._pth",
        ):
            self.assertIn(required, names)
        self.assertTrue(any(n.startswith("Lib/site-packages/_cffi_backend") and n.endswith(".pyd") for n in names))
        with zipfile.ZipFile(VENDOR / "agenthub-libs.zip") as archive:
            pth = archive.read("python314._pth").decode("utf-8")
        self.assertIn("Lib\\site-packages", pth)

    def test_third_party_license_files_are_bundled(self):
        for name in ("CRYPTOGRAPHY-LICENSE.txt", "CFFI-LICENSE.txt", "PYCPARSER-LICENSE.txt"):
            text = (ROOT / "docs/third-party" / name).read_text(encoding="utf-8")
            self.assertTrue(text.strip(), name)


if __name__ == "__main__":
    unittest.main()
