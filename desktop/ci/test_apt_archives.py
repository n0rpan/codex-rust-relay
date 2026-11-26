import hashlib
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from apt_archives import stage


class PackageArchiveCache(unittest.TestCase):
    def test_only_selected_regular_files_matching_the_signed_sha256_are_staged(self):
        content = b"Ubuntu package fixture"
        digest = hashlib.sha256(content).hexdigest()
        with tempfile.TemporaryDirectory() as root:
            cache = Path(root) / "cache"
            cache.mkdir()
            (cache / "valid.deb").write_bytes(content)
            (cache / "corrupt.deb").write_bytes(b"X" * len(content))
            (cache / "stale.deb").write_bytes(content)
            (cache / "linked.deb").symlink_to("valid.deb")
            (cache / "weak.deb").write_bytes(content)
            plan = "\n".join(
                f"'https://example.invalid/{name}' {name} {len(content)} {checksum}"
                for name, checksum in [
                    ("valid.deb", f"SHA256:{digest}"), ("corrupt.deb", f"SHA256:{digest}"),
                    ("linked.deb", f"SHA256:{digest}"), ("weak.deb", f"MD5Sum:{digest}"),
                ]
            )
            with patch("apt_archives.subprocess.check_output", return_value=plan) as apt:
                stage(cache, ["example"])
            self.assertIn("Acquire::ForceHash=SHA256", apt.call_args.args[0])
            self.assertEqual(list(cache.iterdir()), [cache / "valid.deb"])
            self.assertEqual((cache / "valid.deb").read_bytes(), content)
