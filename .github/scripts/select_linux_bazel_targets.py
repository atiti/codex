#!/usr/bin/env python3
"""Partition the existing Linux Bazel test labels without losing coverage."""

import argparse
import sys


def partition(targets: list[str]) -> tuple[list[str], list[str]]:
    if not targets or any(not target.startswith("//") for target in targets):
        raise ValueError("expected a nonempty list of Bazel labels")
    if len(targets) != len(set(targets)):
        raise ValueError("duplicate Bazel labels")
    wine = [
        target
        for target in targets
        if "wine" in target or "/remote_env_windows:" in target
    ]
    native = [target for target in targets if target not in wine]
    if not wine or not native:
        raise ValueError("both partitions must contain tests")
    return sorted(native), sorted(wine)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("partition", choices=("native", "wine"))
    args = parser.parse_args()
    try:
        native, wine = partition(sys.stdin.read().splitlines())
    except ValueError as error:
        parser.error(str(error))
    selected = native if args.partition == "native" else wine
    print("\n".join(selected))
    print(
        f"Selected {len(selected)} of {len(native) + len(wine)} tests ({args.partition})",
        file=sys.stderr,
    )


if __name__ == "__main__":
    main()
