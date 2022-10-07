#!/bin/env python3
import os
import subprocess
import sys


def local(*args):
    subprocess.run(list(args), check=True)


def ssh(remote, *args):
    subprocess.run(["ssh", remote] + list(args), check=True, capture_output=True)


def main(args):
    local("cargo", "build")

    remote = args[1]
    print(f"Copying file to {remote}...")
    subprocess.run(
        ["scp", "target/debug/fwd", f"{remote}:bin/fwd"],
        check=True,
        capture_output=True,
    )

    print(f"Starting process...")
    subprocess.run(["target/debug/fwd", remote])


if __name__ == "__main__":
    main(sys.argv)
