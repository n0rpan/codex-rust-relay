"""Choose extra PR checks from the delta main would receive, including deleted paths."""
import json
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
        "desktop/gate.toml", "desktop/ci/linux-deps.sh", "desktop/ci/apt_archives.py", "desktop/ci/test_apt_archives.py",
        "desktop/ci/browser-runtime.sh",
        "desktop/ci/*protocol.sh", "desktop/ci/quarantine.*", "desktop/ci/wayland-smoke.sh",
        "desktop/ox-desk-probe/*", "desktop/tests/*_window.py", "desktop/tests/packaged-window*",
        "desktop/tests/probe_input.py", "desktop/tests/stock_*.py", "desktop/tests/gtk_drop.py",
        "desktop/tests/claude_stock_session.py",
    )
    build_inputs = (
        "Cargo.*", "build.rs", ".cargo/*", "rust-toolchain*", "desktop/Cargo.*",
        "desktop/build.rs", "desktop/.cargo/*", "desktop/rust-toolchain*",
        "desktop/ox-*/Cargo.toml", "desktop/ox-*/build.rs",
    )
    # ox-desktop and the browser's real server share these local build inputs.
    shared = (*build_inputs, "desktop/ox-desk-core/src/*", "desktop/ox-pty/src/*",
              "src/claude_worker_contract.rs", "src/settings_protocol*",
              ".github/workflows/ci.yml", "desktop/ci/pr_scope.py")
    mac_desktop = (*shared, "desktop/ox-desktop/*", "desktop/ox-host-service/src/*",
                   "desktop/ox-host-web/src/*", "src/version.rs")
    # The phone settings tests launch the ox CLI against a temporary configuration.
    browser = (*shared, "desktop/ox-host-web/*", "src/*", "desktop/ci/browser-runtime.sh")
    native_tests = (
        *build_inputs, "src/*.rs", "tests/*.rs", "desktop/ox-*/*.rs",
        "desktop/ox-*/tests/*", "desktop/tests/*",
        "desktop/ci/*", ".github/workflows/*",
    )

    def touched(patterns):
        return any(fnmatchcase(path, pattern) for path in paths for pattern in patterns)

    mac_package = touched((*packaging, "desktop/tests/*.swift", "desktop/ox-desk-probe/src/gate.py",
                           "desktop/gate.toml"))
    packages = mac_package or full_ci or touched(window_tests)
    # The package runs the same tests on its platform. Exercise the focused workflow
    # itself when it or its selector changes, even if this PR also changes packaging.
    verify_focus = touched((".github/workflows/ci.yml", "desktop/ci/pr_scope.py"))
    focused = []
    if verify_focus or (not mac_package and (full_ci or touched(mac_desktop))):
        focused.append("macos-26")
    if verify_focus or (not packages and touched(browser)):
        focused.append("ubuntu-24.04")
    return {"native": full_ci or touched(native_tests), "packages": packages,
            "mac_package": mac_package, "focused": focused}


if __name__ == "__main__":
    raw = subprocess.check_output(["git", "diff", "--no-renames", "--name-only", "-z",
                                   "HEAD^1", "HEAD", "--"])
    paths = [os.fsdecode(p) for p in raw.split(b"\0") if p]
    flags = lanes(paths, os.environ.get("FULL_CI") == "true")
    with Path(os.environ["GITHUB_OUTPUT"]).open("a") as out:
        for key, value in flags.items():
            print(f"{key}={json.dumps(value)}", file=out)
