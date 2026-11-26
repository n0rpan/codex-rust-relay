import unittest

from pr_scope import lanes


class PullRequestLanes(unittest.TestCase):
    def test_docs_only_still_use_the_fast_lane(self):
        self.assertEqual(lanes(["README.md"], False), (False, False, False))

    def test_native_code_and_its_fixtures_get_native_tests(self):
        for path in ("desktop/ox-host/src/engine.rs", "src/workers/jobs.rs",
                     "desktop/ox-desk-core/tests/fixtures/claude-link.sh"):
            with self.subTest(path=path):
                self.assertEqual(lanes([path], False), (True, False, False))

    def test_window_tests_and_label_select_linux_package(self):
        self.assertEqual(lanes(["desktop/tests/mixed_runtime_window.py"], False), (True, True, False))
        self.assertEqual(lanes(["README.md"], True), (True, True, False))

    def test_shared_packaging_selects_both_platforms(self):
        for path in ("desktop/package.sh", "desktop/packaging/Info.plist", "install.sh",
                     ".github/workflows/desktop.yml", "desktop/tests/packaged-window.swift"):
            with self.subTest(path=path):
                self.assertEqual(lanes([path], False)[1:], (True, True))
