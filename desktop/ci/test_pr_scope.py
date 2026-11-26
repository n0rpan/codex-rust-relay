import unittest

from pr_scope import lanes


class PullRequestLanes(unittest.TestCase):
    def test_docs_only_still_use_the_fast_lane(self):
        flags = lanes(["README.md"], False)
        self.assertFalse(any(flags.values()))

    def test_native_code_and_its_fixtures_get_native_tests(self):
        for path in ("desktop/ox-host/src/engine.rs", "src/workers/jobs.rs",
                     "desktop/ox-desk-core/tests/fixtures/claude-link.sh"):
            with self.subTest(path=path):
                flags = lanes([path], False)
                self.assertTrue(flags["native"])
                self.assertFalse(flags["packages"])

    def test_window_tests_and_label_select_linux_package(self):
        self.assertTrue(lanes(["desktop/tests/mixed_runtime_window.py"], False)["packages"])
        self.assertEqual(lanes(["README.md"], True),
                         {"native": True, "packages": True, "mac_package": False,
                          "focused": ["macos-26"]})

    def test_shared_packaging_selects_both_platforms(self):
        for path in ("desktop/package.sh", "desktop/packaging/Info.plist", "install.sh",
                     "src/update.rs", "src/update/desktop.rs", "src/managed_install.rs",
                     ".github/workflows/desktop.yml", "desktop/tests/packaged-window.swift",
                     "desktop/ox-desk-probe/src/gate.py", "desktop/gate.toml"):
            with self.subTest(path=path):
                flags = lanes([path], False)
                self.assertTrue(flags["packages"] and flags["mac_package"])
                self.assertFalse(flags["focused"])

    def test_macos_implements_the_desktop_platform_paths(self):
        for path in ("desktop/ox-desktop/src/platform.rs", "desktop/ox-desktop/src/platform/probe.rs",
                     "desktop/ox-desktop/src/terminal.rs"):
            with self.subTest(path=path):
                flags = lanes([path], False)
                self.assertEqual(flags["focused"], ["macos-26"])
                self.assertFalse(flags["packages"])

    def test_browser_inputs_run_the_browser_itself(self):
        for path in ("desktop/ox-host-web/assets/operator.js", "desktop/ox-host-web/assets/style.css",
                     "desktop/ox-host-web/tests/operator_browser.py", "desktop/ox-host-web/tests/check.sh"):
            with self.subTest(path=path):
                flags = lanes([path], False)
                self.assertEqual(flags["focused"], ["ubuntu-24.04"])
                self.assertFalse(flags["packages"])

    def test_build_inputs_exercise_both_consumers(self):
        for path in ("Cargo.lock", "desktop/Cargo.lock", "desktop/ox-desktop/Cargo.toml",
                     "desktop/ox-pty/build.rs", "build.rs", "desktop/.cargo/config.toml",
                     "desktop/ox-desk-core/src/host_protocol.rs"):
            with self.subTest(path=path):
                flags = lanes([path], False)
                self.assertTrue(flags["native"])
                self.assertCountEqual(flags["focused"], ["macos-26", "ubuntu-24.04"])
                self.assertFalse(flags["packages"])

    def test_linux_window_package_still_needs_a_separate_macos_gui_check(self):
        flags = lanes(["desktop/tests/mixed_runtime_window.py", "desktop/ox-desktop/src/main.rs",
                       "desktop/ox-host-web/assets/index.html"], False)
        self.assertTrue(flags["packages"])
        self.assertFalse(flags["mac_package"])
        self.assertEqual(flags["focused"], ["macos-26"])

    def test_ci_workflow_or_selector_runs_focused_jobs_even_when_packaging_runs(self):
        for path in (".github/workflows/ci.yml", "desktop/ci/pr_scope.py"):
            with self.subTest(path=path):
                flags = lanes([path, "desktop/package.sh"], False)
                self.assertTrue(flags["packages"] and flags["mac_package"])
                self.assertCountEqual(flags["focused"], ["macos-26", "ubuntu-24.04"])

    def test_package_archive_checks_run_in_the_linux_package_lane(self):
        for path in ("desktop/ci/apt_archives.py", "desktop/ci/test_apt_archives.py"):
            flags = lanes([path], False)
            self.assertTrue(flags["packages"])
            self.assertFalse(flags["mac_package"])
