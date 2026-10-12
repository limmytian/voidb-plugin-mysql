#!/usr/bin/env python3
"""Turn exact-source unsigned build artifacts into new signed release candidates."""
import argparse
import hashlib
import importlib.util
import os
from pathlib import Path, PurePosixPath
import subprocess
import tarfile
import tempfile
import tomllib

TARGETS = ["x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu", "x86_64-apple-darwin", "aarch64-apple-darwin"]


def load_script(filename):
    spec = importlib.util.spec_from_file_location(filename, Path(__file__).with_name(filename))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def unpack(archive, directory):
    total = 0
    with tarfile.open(archive) as source:
        for item in source:
            relative = PurePosixPath(item.name)
            if relative.is_absolute() or ".." in relative.parts or "\\" in item.name or ":" in item.name:
                raise ValueError("Unsafe archive path")
            destination = directory.joinpath(*relative.parts)
            if item.isdir():
                destination.mkdir(parents=True, exist_ok=True)
            elif item.isfile():
                total += item.size
                if total > 1024 * 1024 * 1024:
                    raise ValueError("Release payload exceeds 1 GiB")
                destination.parent.mkdir(parents=True, exist_ok=True)
                with source.extractfile(item) as stream:
                    destination.write_bytes(stream.read())
                destination.chmod(item.mode & 0o755)
            else:
                raise ValueError("Release payload contains links or nonregular files")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    parser.add_argument("--plugin-id", required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--target", action="append", choices=TARGETS)
    parser.add_argument("--key-file", type=Path)
    parser.add_argument("--openssl", default="openssl")
    args = parser.parse_args()
    if tuple(map(int, args.version.split("."))) < (0, 3, 4):
        parser.error("Published unsigned releases must remain immutable; use a new version >=0.3.4")
    signer = load_script("sign-plugin-provenance.py")
    verifier = load_script("verify-plugin-provenance.py")
    with tempfile.TemporaryDirectory(prefix="voidb-release-sign-") as temporary:
        temporary = Path(temporary)
        key = args.key_file
        if key is None:
            pem = os.environ.get("VOIDB_PLUGIN_SIGNING_KEY_PEM")
            if not pem:
                parser.error("Signing key file or protected secret environment is required")
            key = temporary / "key.pem"
            descriptor = os.open(key, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
            with os.fdopen(descriptor, "w") as output:
                output.write(pem)
        for target in args.target or TARGETS:
            basename = f"{args.plugin_id}-{args.version}-{target}"
            stage = temporary / target
            stage.mkdir()
            unpack(args.directory / (basename + ".tar.gz"), stage)
            manifest = tomllib.loads((stage / "plugin.toml").read_text())
            if (manifest["id"], manifest["version"]) != (args.plugin_id, args.version):
                raise ValueError("Build artifact identity does not match release tag")
            signer.sign_package(stage, target, key, args.openssl)
            verifier.verify(stage, target, args.plugin_id, args.version, args.openssl)
            for suffix, mode in [(".tar", "w"), (".tar.gz", "w:gz")]:
                archive = args.directory / (basename + suffix)
                with tarfile.open(archive, mode) as output:
                    for path in sorted(stage.iterdir()):
                        output.add(path, arcname=path.name)
            tar_path = args.directory / (basename + ".tar")
            zst_path = args.directory / (basename + ".tar.zst")
            subprocess.run(["zstd", "-q", "-f", str(tar_path), "-o", str(zst_path)], check=True)
            for suffix in [".tar", ".tar.gz", ".tar.zst"]:
                archive = args.directory / (basename + suffix)
                signature = subprocess.check_output([args.openssl, "pkeyutl", "-sign", "-rawin", "-inkey", str(key), "-in", str(archive)])
                Path(str(archive) + ".sig").write_text(signature.hex() + "\n")
    files = sorted(path for path in args.directory.iterdir() if path.is_file() and path.name != "SHA256SUMS")
    (args.directory / "SHA256SUMS").write_text("".join(f"{hashlib.sha256(path.read_bytes()).hexdigest()}  {path.name}\n" for path in files))


if __name__ == "__main__":
    main()
