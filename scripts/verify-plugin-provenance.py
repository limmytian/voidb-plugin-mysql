#!/usr/bin/env python3
"""Verify a signed release payload for an explicitly declared target."""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import tempfile
import tomllib
import importlib.util
_spec = importlib.util.spec_from_file_location("voidb_sign", Path(__file__).with_name("sign-plugin-provenance.py"))
_signer = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_signer)
DOMAIN, PUBLIC_KEY = _signer.DOMAIN, _signer.PUBLIC_KEY


def verify(directory, target, plugin_id, version, openssl):
    directory = directory.resolve(strict=True)
    proof = json.loads((directory / "provenance.json").read_text())
    if set(proof) != {"key_id", "payload", "signature"} or proof["key_id"] != "official-2026-10":
        raise ValueError("Unknown provenance signer or envelope fields")
    payload = json.loads(proof["payload"])
    if set(payload) != {"schema_version", "plugin_id", "version", "protocol_version", "target", "binary_path", "files"}:
        raise ValueError("Unexpected provenance fields")
    manifest = tomllib.loads((directory / "plugin.toml").read_text())
    if payload["schema_version"] != 1 or payload["target"] != target or payload["plugin_id"] != plugin_id or payload["version"] != version:
        raise ValueError("Provenance does not match release identity")
    if (manifest["id"], manifest["version"], manifest["protocol_version"]) != (plugin_id, version, payload["protocol_version"]):
        raise ValueError("Manifest identity does not match proof")
    command = manifest["runtime"]["command"]
    binary = Path(command) if "/" in command else Path("bin") / command
    if binary.is_absolute() or ".." in binary.parts or payload["binary_path"] != binary.as_posix():
        raise ValueError("Invalid signed executable path")
    actual = {}
    for path in sorted(directory.rglob("*")):
        if path.is_symlink() or not (path.is_file() or path.is_dir()):
            raise ValueError("Nonregular payload")
        if path.is_file() and path.relative_to(directory).as_posix() != "provenance.json":
            actual[path.relative_to(directory).as_posix()] = "sha256:" + hashlib.sha256(path.read_bytes()).hexdigest()
    if actual != payload["files"] or binary.as_posix() not in actual or "plugin.toml" not in actual:
        raise ValueError("Signed payload digest mismatch")
    with tempfile.TemporaryDirectory(prefix="voidb-verify-") as temporary:
        root = Path(temporary)
        (root / "public.der").write_bytes(bytes.fromhex("302a300506032b6570032100" + PUBLIC_KEY))
        (root / "message").write_bytes(DOMAIN + proof["payload"].encode())
        (root / "signature").write_bytes(bytes.fromhex(proof["signature"]))
        subprocess.run([openssl, "pkeyutl", "-verify", "-rawin", "-pubin", "-keyform", "DER",
                        "-inkey", str(root / "public.der"), "-in", str(root / "message"),
                        "-sigfile", str(root / "signature")], check=True, stdout=subprocess.DEVNULL)
    return payload


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    parser.add_argument("--target", required=True)
    parser.add_argument("--plugin-id", required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--openssl", default="openssl")
    args = parser.parse_args()
    verify(args.directory, args.target, args.plugin_id, args.version, args.openssl)
    print(f"Verified {args.plugin_id} {args.version} for {args.target}")


if __name__ == "__main__":
    main()
