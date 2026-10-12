#!/usr/bin/env python3
"""Sign a complete plugin payload without placing private key material in argv."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile
import tomllib

DOMAIN = b"voidb-plugin-provenance-v1\n"
PUBLIC_KEY = "2d00e2e461719c94b77cf0e9d1d49a9cf1c8dbe8f802696b801b432bfcc95ea2"


def sign_package(directory, target, key_file, openssl):
    directory = directory.resolve(strict=True)
    if key_file.resolve(strict=True).is_relative_to(directory):
        raise ValueError("Private signing keys must stay outside plugin payload directories")
    if key_file.stat().st_nlink != 1:
        raise ValueError("Private signing keys must not be hard-linked into other locations")
    manifest = tomllib.loads((directory / "plugin.toml").read_text())
    command = manifest["runtime"]["command"]
    binary = Path(command) if "/" in command else Path("bin") / command
    if binary.is_absolute() or ".." in binary.parts or not (directory / binary).is_file():
        raise ValueError("Executable must be package-relative and present")
    files = {}
    for path in sorted(directory.rglob("*")):
        if path.is_symlink():
            raise ValueError("Signed packages cannot contain symlinks")
        if path.is_dir():
            continue
        if not path.is_file():
            raise ValueError("Signed packages contain only regular files")
        relative = path.relative_to(directory).as_posix()
        if relative == "provenance.json":
            continue
        if "\\" in relative or ":" in relative:
            raise ValueError("Invalid payload path")
        files[relative] = "sha256:" + hashlib.sha256(path.read_bytes()).hexdigest()
    payload = json.dumps({
        "schema_version": 1,
        "plugin_id": manifest["id"],
        "version": manifest["version"],
        "protocol_version": manifest["protocol_version"],
        "target": target,
        "binary_path": binary.as_posix(),
        "files": files,
    }, sort_keys=True, separators=(",", ":"), ensure_ascii=False)
    if key_file.is_symlink() or not key_file.is_file() or key_file.stat().st_mode & 0o077:
        raise ValueError("Private key must be a regular file with owner-only permissions")
    public = subprocess.check_output([
        openssl, "pkey", "-in", str(key_file), "-pubout", "-outform", "DER"
    ])
    if public != bytes.fromhex("302a300506032b6570032100" + PUBLIC_KEY):
        raise ValueError("Signing key does not match the compiled official release root")
    with tempfile.TemporaryDirectory(prefix="voidb-sign-") as temporary:
        message = Path(temporary) / "message"
        message.write_bytes(DOMAIN + payload.encode())
        signature = subprocess.check_output([
            openssl, "pkeyutl", "-sign", "-rawin", "-inkey", str(key_file), "-in", str(message)
        ]).hex()
    envelope = {"key_id": "official-2026-10", "payload": payload, "signature": signature}
    (directory / "provenance.json").write_text(json.dumps(envelope, indent=2) + "\n")
    print(f"Signed {manifest['id']} {manifest['version']} for {target} ({len(files)} files)")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    parser.add_argument("--target", required=True)
    parser.add_argument("--key-file", type=Path)
    parser.add_argument("--openssl", default="openssl")
    args = parser.parse_args()
    # Protected CI jobs supply a PEM through their secret environment. Never echo it.
    if args.key_file:
        sign_package(args.directory, args.target, args.key_file, args.openssl)
    else:
        pem = os.environ.get("VOIDB_PLUGIN_SIGNING_KEY_PEM")
        if not pem:
            parser.error("Provide --key-file or VOIDB_PLUGIN_SIGNING_KEY_PEM")
        with tempfile.TemporaryDirectory(prefix="voidb-key-") as temporary:
            key_file = Path(temporary) / "key.pem"
            descriptor = os.open(key_file, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
            with os.fdopen(descriptor, "w") as output:
                output.write(pem)
            sign_package(args.directory, args.target, key_file, args.openssl)


if __name__ == "__main__":
    main()
