import unittest

from select_linux_bazel_targets import partition


class LinuxBazelPartitionTest(unittest.TestCase):
    def test_partitions_are_disjoint_and_complete(self):
        targets = [
            "//codex-rs/core:core-all-test",
            "//bazel/rules/testing/wine:wine-test-support-unit-tests",
            "//codex-rs/core:core-windows_sandbox-wine-exec-test",
            "//codex-rs/core/tests/remote_env_windows:smoke-test",
        ]
        native, wine = partition(targets)
        self.assertEqual(set(native) | set(wine), set(targets))
        self.assertFalse(set(native) & set(wine))
        self.assertEqual(native, targets[:1])
        self.assertEqual(len(wine), 3)

    def test_invalid_input_fails_closed(self):
        for labels in ([], ["invalid"], ["//x:a", "//x:a"], ["//x:a"]):
            with self.subTest(labels=labels), self.assertRaises(ValueError):
                partition(labels)


if __name__ == "__main__":
    unittest.main()
