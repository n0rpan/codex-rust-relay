import hashlib
from pathlib import Path
import subprocess
import tempfile
import unittest

SCRIPT = Path(__file__).with_name("release-assets.sh")
VERSION = "1.2.3-dev.45"
DESKTOP = {"ubuntu-24.04": "linux-x86_64.tar.gz", "ubuntu-24.04-arm": "linux-aarch64.tar.gz",
           "macos-26": "macos-arm64.zip", "macos-26-intel": "macos-x86_64.zip"}
TARGETS = ("x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu",
           "x86_64-apple-darwin", "aarch64-apple-darwin")


def expected(signed):
    """The release's files below the artifact directory, in upload order."""
    mac = "signed-" if signed else ""
    names = []
    for runner, suffix in DESKTOP.items():
        names.append(f"ox-desktop-{mac if runner.startswith('macos') else ''}{runner}/ox-desktop-{suffix}")
    for target in TARGETS:
        artifact = f"ox-{mac if target.endswith('-apple-darwin') else ''}{target}"
        names += [f"{artifact}/ox-{VERSION}-{target}", f"{artifact}/ox-{VERSION}-{target}.tar.gz",
                  f"{artifact}/ox-{target}.tar.gz"]
    return [name + end for name in names for end in ("", ".sha256")]


def download(root, signed):
    """What `gh run download --pattern 'ox-*'` leaves: one directory per artifact."""
    for name in expected(signed)[::2]:
        path = Path(root, name)
        path.parent.mkdir(exist_ok=True)
        path.write_bytes(name.encode())
        digest = hashlib.sha256(name.encode()).hexdigest()
        Path(root, name + ".sha256").write_text(f"{digest}  {path.name}\n")


def run(root, *options):
    return subprocess.run(["bash", str(SCRIPT), *options, root, VERSION],
                          capture_output=True, text=True, check=False)


class ReleaseAssets(unittest.TestCase):
    def test_a_release_run_lists_the_signed_macos_files_in_upload_order(self):
        with tempfile.TemporaryDirectory() as root:
            download(root, signed=True)
            done = run(root)
            self.assertEqual(done.returncode, 0, done.stderr)
            self.assertEqual(done.stdout.splitlines(), [f"{root}/{name}" for name in expected(True)])
            self.assertEqual(len(done.stdout.splitlines()), 32)

    def test_a_pull_request_finds_the_same_files_in_the_unsigned_artifacts(self):
        with tempfile.TemporaryDirectory() as root:
            download(root, signed=False)
            done = run(root, "--unsigned")
            self.assertEqual(done.returncode, 0, done.stderr)
            self.assertEqual(done.stdout.splitlines(), [f"{root}/{name}" for name in expected(False)])
            self.assertEqual([Path(line).name for line in done.stdout.splitlines()],
                             [Path(name).name for name in expected(True)])

    def test_a_release_never_falls_back_to_the_unsigned_macos_builds(self):
        with tempfile.TemporaryDirectory() as root:
            download(root, signed=False)
            done = run(root)
            self.assertEqual(done.returncode, 1)
            self.assertEqual(done.stdout, "")
            for name in expected(True):
                self.assertEqual("signed-" in name, f"missing: {root}/{name}\n" in done.stderr, name)

    def test_each_missing_file_is_named_and_nothing_is_listed(self):
        for signed in (True, False):
            for name in expected(signed):
                with self.subTest(name=name), tempfile.TemporaryDirectory() as root:
                    download(root, signed)
                    Path(root, name).unlink()
                    done = run(root, *([] if signed else ["--unsigned"]))
                    self.assertEqual(done.returncode, 1)
                    self.assertEqual(done.stdout, "")
                    self.assertIn(f"::error::release asset missing: {root}/{name}\n", done.stderr)

    def test_each_file_that_does_not_match_its_checksum_is_named(self):
        for name in expected(True):
            with self.subTest(name=name), tempfile.TemporaryDirectory() as root:
                download(root, signed=True)
                path = Path(root, name)
                first = path.read_bytes()[:1]
                with path.open("r+b") as file:
                    file.write(b"1" if first == b"0" else b"0")
                done = run(root)
                self.assertEqual(done.returncode, 1)
                self.assertEqual(done.stdout, "")
                asset = name.removesuffix(".sha256")
                self.assertIn(f"::error::release asset does not match its checksum: {root}/{asset}\n",
                              done.stderr)

    def test_a_build_of_another_version_is_not_published_under_this_one(self):
        with tempfile.TemporaryDirectory() as root:
            download(root, signed=True)
            done = subprocess.run(["bash", str(SCRIPT), root, "1.2.3"],
                                  capture_output=True, text=True, check=False)
            self.assertEqual(done.returncode, 1)
            self.assertIn(f"missing: {root}/ox-x86_64-unknown-linux-gnu/ox-1.2.3-x86_64-unknown-linux-gnu\n",
                          done.stderr)

    def test_the_workflows_upload_an_artifact_under_every_name_a_release_reads(self):
        # The signing jobs run only on main, so a rename there is otherwise first seen by a lost release.
        workflows = Path(__file__).parents[2] / ".github" / "workflows"
        uploads = (workflows / "ci.yml").read_text() + (workflows / "desktop.yml").read_text()
        for name in expected(True) + expected(False):
            artifact = name.split("/")[0]
            if "signed" not in artifact:
                artifact = ("ox-desktop-${{ matrix.runner }}" if artifact.startswith("ox-desktop-")
                            else "ox-${{ matrix.target }}")
            with self.subTest(artifact=artifact):
                self.assertIn(f"\n          name: {artifact}\n", uploads)

    def test_a_missing_version_or_directory_is_a_usage_error(self):
        for arguments in ([], ["dist"], ["dist", ""], ["--unsigned", "dist"], ["dist", VERSION, "extra"]):
            with self.subTest(arguments=arguments):
                done = subprocess.run(["bash", str(SCRIPT), *arguments],
                                      capture_output=True, text=True, check=False)
                self.assertEqual(done.returncode, 2)
                self.assertIn("usage:", done.stderr)


if __name__ == "__main__":
    unittest.main()
