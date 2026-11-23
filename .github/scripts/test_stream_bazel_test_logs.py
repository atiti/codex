import subprocess
import sys
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory

import stream_bazel_test_logs as stream


class StreamBazelTestLogsTest(unittest.TestCase):
    def test_attempt_is_emitted_before_command_finishes_and_failure_is_preserved(self):
        with TemporaryDirectory() as temporary:
            root = Path(temporary)
            attempt = root / "bazel-testlogs/shard_2_of_16/test_attempts/attempt_1.log"
            attempt.parent.mkdir(parents=True)
            attempt.write_text("specific fixture assertion\n", encoding="utf-8")
            child = (
                "import os, sys, threading; "
                "timer = threading.Timer(15, lambda: os._exit(99)); "
                "timer.daemon = True; timer.start(); "
                f"print('FAIL: //app-server:test (see {attempt})', flush=True); "
                "sys.stdin.readline(); sys.exit(37)"
            )
            with subprocess.Popen(
                [sys.executable, stream.__file__, "--", sys.executable, "-c", child],
                cwd=root,
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
            ) as process:
                output = []
                while True:
                    line = process.stdout.readline()
                    self.assertTrue(line, "wrapper exited before emitting attempt")
                    output.append(line)
                    if "--- End Bazel failed attempt ---" in line:
                        break
                self.assertIn("| specific fixture assertion\n", output)
                self.assertIsNone(process.poll())
                process.stdin.write("finish\n")
                process.stdin.flush()
                self.assertEqual(process.wait(timeout=10), 37)

    def test_output_is_bounded_and_duplicate_attempts_are_suppressed(self):
        from contextlib import redirect_stdout
        from io import StringIO

        with TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            attempt = root / "test_attempts/attempt_2.log"
            attempt.parent.mkdir()
            attempt.write_text(
                "omitted first line\n"
                + "x" * stream.MAX_BYTES
                + "\n::error::fixture tail\n"
            )
            output = StringIO()
            seen = set()
            with redirect_stdout(output):
                stream.print_attempt(
                    f"\033[31mFAIL: target (see {attempt})\033[0m", root, seen
                )
                stream.print_attempt(f"FAIL: target (see {attempt})", root, seen)
            text = output.getvalue()
            self.assertNotIn("omitted first line", text)
            self.assertIn("| ::error::fixture tail", text)
            self.assertEqual(text.count("--- End Bazel failed attempt ---"), 1)
            self.assertLess(len(text), stream.MAX_BYTES + 1000)

    def test_untrusted_paths_and_symlink_escapes_are_not_read(self):
        from contextlib import redirect_stdout
        from io import StringIO

        with TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            allowed = root / "bazel-testlogs"
            allowed.mkdir()
            secret = root / "test_attempts/attempt_1.log"
            secret.parent.mkdir()
            secret.write_text("private content")
            (allowed / "escape").symlink_to(root, target_is_directory=True)
            output = StringIO()
            with redirect_stdout(output):
                for path in (secret, allowed / "escape/test_attempts/attempt_1.log"):
                    stream.print_attempt(f"FAIL: target (see {path})", allowed, set())
            self.assertEqual(output.getvalue(), "")


if __name__ == "__main__":
    unittest.main()
