"""Release proofs must reject substituted content using independent test keys."""
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


def load(filename):
    spec = importlib.util.spec_from_file_location(filename, Path(__file__).parents[1] / filename)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class ProvenanceTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.package = self.root / "package"
        (self.package / "bin").mkdir(parents=True)
        (self.package / "schemas").mkdir()
        (self.package / "plugin.toml").write_text('''id = "fixture"
version = "0.3.4"
protocol_version = "1"
[runtime]
command = "fixture"
''')
        (self.package / "bin/fixture").write_bytes(b"independent test payload")
        (self.package / "schemas/profile.json").write_text('{"type":"object"}')
        self.openssl = os.environ.get("OPENSSL_EXECUTABLE", shutil.which("openssl"))
        self.key = self.root / "key.pem"
        subprocess.run([self.openssl, "genpkey", "-algorithm", "ED25519", "-out", str(self.key)], check=True)
        self.key.chmod(0o600)
        public = subprocess.check_output([self.openssl, "pkey", "-in", str(self.key), "-pubout", "-outform", "DER"])[-32:].hex()
        self.signer = load("sign-plugin-provenance.py")
        self.verifier = load("verify-plugin-provenance.py")
        # Test-only policy injection never changes the production script or core roots.
        self.signer.PUBLIC_KEY = self.verifier.PUBLIC_KEY = public
        self.signer.sign_package(self.package, "aarch64-apple-darwin", self.key, self.openssl)

    def verify(self):
        return self.verifier.verify(self.package, "aarch64-apple-darwin", "fixture", "0.3.4", self.openssl)

    def test_valid_signature_and_binary_schema_substitution(self):
        self.verify()
        for name in ["bin/fixture", "schemas/profile.json"]:
            original = (self.package / name).read_bytes()
            (self.package / name).write_bytes(b"substituted")
            with self.assertRaises(ValueError):
                self.verify()
            (self.package / name).write_bytes(original)
        self.verify()

    def test_editing_payload_digests_cannot_forge_a_signature(self):
        proof_path = self.package / "provenance.json"
        proof = json.loads(proof_path.read_text())
        payload = json.loads(proof["payload"])
        payload["files"]["schemas/profile.json"] = "sha256:" + "0" * 64
        proof["payload"] = json.dumps(payload)
        proof_path.write_text(json.dumps(proof))
        with self.assertRaises(ValueError):
            self.verify()

    def test_bad_signature_and_unknown_signer_are_rejected(self):
        proof_path = self.package / "provenance.json"
        original = json.loads(proof_path.read_text())
        proof = dict(original, signature="00" * 64)
        proof_path.write_text(json.dumps(proof))
        with self.assertRaises(subprocess.CalledProcessError):
            self.verify()
        proof_path.write_text(json.dumps(dict(original, key_id="unknown")))
        with self.assertRaises(ValueError):
            self.verify()

    def test_private_key_cannot_be_packaged_with_the_payload(self):
        internal = self.package / "private.pem"
        internal.write_bytes(self.key.read_bytes())
        internal.chmod(0o600)
        with self.assertRaisesRegex(ValueError, 'outside plugin payload'):
            self.signer.sign_package(self.package, "aarch64-apple-darwin", internal, self.openssl)

    def test_extra_files_and_symlinked_payload_are_rejected(self):
        extra = self.package / "extra"
        extra.write_bytes(b"unsigned")
        with self.assertRaises(ValueError):
            self.verify()
        extra.unlink()
        binary = self.package / "bin/fixture"
        target = self.root / "outside"
        binary.rename(target)
        try:
            binary.symlink_to(target)
        except OSError:
            self.skipTest("Host does not permit test symlinks")
        with self.assertRaises(ValueError):
            self.verify()


if __name__ == "__main__":
    unittest.main()
