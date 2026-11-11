import pathlib
import json
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

    def test_release_config_contains_runtime_and_license_resources(self):
        config = json.loads((ROOT / "src-tauri" / "tauri.conf.json").read_text(encoding="utf-8"))
        resources = config["bundle"]["resources"]
        self.assertEqual(resources["../LICENSE"], "LICENSE")
        self.assertEqual(resources["../THIRD_PARTY_NOTICES.md"], "THIRD_PARTY_NOTICES.md")
        self.assertEqual(resources["../docs/third-party/"], "licenses/")

    def test_bundled_agenthub_has_no_plaintext_cookie_cache(self):
        source = (VENDOR / "agenthub.py").read_text(encoding="utf-8")
        self.assertNotIn("DOUBAO_CACHE", source)
        self.assertNotIn('WORKBUDDY_AT_REST_SECRET_KEY = "', source)

    def test_workbuddy_runtime_key_recovery_is_local_and_verified(self):
        source = (ROOT / "src-tauri/src/wbproxy.rs").read_text(encoding="utf-8")
        self.assertIn("fn ensure_local_secret", source)
        self.assertIn("fn load_keyblobs", source)
        self.assertIn("blobs.iter().any", source)
        self.assertIn("validate_keyblob_key", source)
        self.assertIn("PROCESS_VM_READ", source)
        self.assertIn("ReadProcessMemory", source)
        self.assertNotIn("const WORKBUDDY_AT_REST_SECRET_KEY", source)
        main = (ROOT / "src-tauri/src/main.rs").read_text(encoding="utf-8")
        self.assertNotIn('lifecycle::note(&format!("WorkBuddy 密钥暂未更新', main)
        self.assertIn("secret_refreshed = true", main)
        self.assertIn("if slot % 60 == 0 || secret_refreshed", main)


if __name__ == "__main__":
    unittest.main()
