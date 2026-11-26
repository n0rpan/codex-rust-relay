"""Stage only the Ubuntu archives selected by APT and verified against its signed indexes."""
import hashlib
import os
from pathlib import Path
import re
import shlex
import shutil
import stat
import subprocess
import sys
import tempfile


def stage(cache, packages):
    # APT can reuse an existing archive by its filename and size. Give it a clean
    # directory and copy in only the files matching the current transaction.
    with tempfile.TemporaryDirectory(dir=cache.parent) as temp:
        previous = Path(temp) / "previous"
        if cache.exists() or cache.is_symlink():
            cache.rename(previous)
        cache.mkdir(mode=0o755)
        if previous.is_symlink() or not previous.is_dir():
            print("APT archive cache is cold.")
            return

        plan = subprocess.check_output([
            "sudo", "apt-get", "-qq", "--yes", "--print-uris", "-o", "Debug::NoLocking=true",
            "-o", "Acquire::ForceHash=SHA256", "-o", f"Dir::Cache::archives={cache}",
            "install", *packages,
        ], text=True)
        hits = 0
        for line in plan.splitlines():
            if not line.startswith("'"):
                continue
            _, name, size, checksum = shlex.split(line)
            if (Path(name).name != name or not name.endswith(".deb")
                    or not re.fullmatch(r"SHA256:[0-9a-f]{64}", checksum)):
                continue
            source = previous / name
            try:
                info = source.lstat()
            except FileNotFoundError:
                continue
            if not stat.S_ISREG(info.st_mode) or info.st_size != int(size):
                continue
            archive = cache / name
            shutil.copyfile(source, archive)
            with archive.open("rb") as data:
                actual = hashlib.file_digest(data, "sha256").hexdigest()
            if actual != checksum.removeprefix("SHA256:"):
                archive.unlink()
                print(f"Discarded an invalid cached archive: {name}")
                continue
            hits += 1
        print(f"Reused {hits} verified APT archives.")


if __name__ == "__main__":
    stage(Path(os.environ["RUNNER_TEMP"]) / "ox-package-debs", sys.argv[1:])
