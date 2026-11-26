"""Choose extra PR checks from the delta main would receive, including deleted paths."""
import os
import subprocess
from fnmatch import fnmatchcase
from pathlib import Path


def lanes(paths, full_ci):
    packaging = (
        "install.sh", "desktop/package.sh", "desktop/packaging/*",
        "desktop/ci/release-assets.sh", "desktop/tests/host_clean_account.sh",
        "desktop/tests/host_service.py", ".github/workflows/desktop.yml",
    )
    window_tests = (
        "desktop/gate.toml", "desktop/ci/linux-deps.sh", "desktop/ci/browser-runtime.sh",
        "desktop/ci/*protocol.sh", "desktop/ci/quarantine.*", "desktop/ci/wayland-smoke.sh",
        "desktop/ox-desk-probe/*", "desktop/tests/*_window.py", "desktop/tests/packaged-window*",
        "desktop/tests/probe_input.py", "desktop/tests/stock_*.py", "desktop/tests/gtk_drop.py",
        "desktop/tests/claude_stock_session.py",
    )
    native_tests = (
        "src/*.rs", "tests/*.rs", "Cargo.*", "desktop/Cargo.*", "desktop/ox-*/*.rs",
        "desktop/ox-*/Cargo.toml", "desktop/ox-*/tests/*", "desktop/tests/*",
        "desktop/ci/*", ".github/workflows/*",
    )

    def touched(patterns):
        return any(fnmatchcase(path, pattern) for path in paths for pattern in patterns)

    mac_package = touched((*packaging, "desktop/tests/*.swift"))
    packages = mac_package or full_ci or touched(window_tests)
    return full_ci or touched(native_tests), packages, mac_package


if __name__ == "__main__":
    raw = subprocess.check_output(["git", "diff", "--no-renames", "--name-only", "-z",
                                   "HEAD^1", "HEAD", "--"])
    paths = [os.fsdecode(p) for p in raw.split(b"\0") if p]
    flags = lanes(paths, os.environ.get("FULL_CI") == "true")
    with Path(os.environ["GITHUB_OUTPUT"]).open("a") as out:
        for key, flag in zip(("native", "packages", "mac_package"), flags):
            print(f"{key}={str(flag).lower()}", file=out)
