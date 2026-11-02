"""Show failed Bazel retry logs immediately, preserving the command's exit status."""

import re
import subprocess
import sys
from pathlib import Path

ATTEMPT = re.compile(r"\(see (.+/test_attempts/attempt_[0-9]+\.log)\)")
ANSI = re.compile(r"\x1b\[[0-?]*[ -/]*[@-~]")
MAX_BYTES = 64 * 1024
MAX_LINES = 200


def print_attempt(line: str, root: Path, seen: set[Path]) -> None:
    match = ATTEMPT.search(ANSI.sub("", line))
    if match is None:
        return
    try:
        path = Path(match.group(1)).resolve()
    except (OSError, RuntimeError):
        return
    # Test stdout is untrusted. Never read a path outside Bazel's test-log tree,
    # including a symlink escaping that tree.
    if not root.is_dir() or not path.is_relative_to(root) or path in seen:
        return
    try:
        with path.open("rb") as log:
            log.seek(0, 2)
            size = log.tell()
            log.seek(max(0, size - MAX_BYTES))
            content = log.read(MAX_BYTES).decode("utf-8", errors="replace")
    except OSError as error:
        print(f"Bazel attempt log unavailable: {path}: {error}", flush=True)
        return
    seen.add(path)
    print(
        f"--- Bazel failed attempt: {path} (last {MAX_LINES} lines, at most {MAX_BYTES} bytes) ---"
    )
    for text in content.splitlines()[-MAX_LINES:]:
        # Prefix log content so a fixture cannot inject workflow commands.
        print(f"| {text}")
    print("--- End Bazel failed attempt ---", flush=True)


def main(command: list[str]) -> int:
    if command[:1] == ["--"]:
        command = command[1:]
    if not command:
        print("usage: stream_bazel_test_logs.py -- COMMAND [ARG ...]", file=sys.stderr)
        return 2
    root = Path("bazel-testlogs").resolve()
    seen: set[Path] = set()
    with subprocess.Popen(
        command,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        encoding="utf-8",
        errors="replace",
        bufsize=1,
    ) as process:
        assert process.stdout is not None
        for line in process.stdout:
            sys.stdout.write(line)
            sys.stdout.flush()
            print_attempt(line, root, seen)
        result = process.wait()
    return result if result >= 0 else 128 - result


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
