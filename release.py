"""A script to automate building and uploading a release archive.

This is in python instead of bash because I abhor bash. Even though it's a
little nicer for running commands, it's worse at everything else.
"""

import dataclasses
import enum
import os
import os.path
import pathlib
import shutil
import subprocess

RELEASE_TAG = os.getenv("RELEASE_TAG")

BUILD = os.getenv("BUILD")
if BUILD is None:
    raise Exception("you *must* set the BUILD environment variable")


class Archive(enum.Enum):
    TARBALL = 1
    ZIP = 2
    DEB = 3


@dataclasses.dataclass
class BuildSettings:
    target: str  # The rust target to build for
    test: bool = True  # Whether or not to run tests
    man_page: bool = True  # Whether or not to generate a man page
    strip: bool = True  # Whether or not to strip binaries
    archive: Archive = Archive.TARBALL  # Archive type
    ext: str = ""  # The file extension of the binary


print(f"doing release: {BUILD}")
build = {
    "linux": BuildSettings(
        target="x86_64-unknown-linux-musl",
    ),
    "deb": BuildSettings(
        target="x86_64-unknown-linux-musl",
        test=False,
        archive=Archive.DEB,
    ),
    "macos": BuildSettings(
        target="x86_64-apple-darwin",
    ),
    "arm-macos": BuildSettings(
        target="aarch64-apple-darwin",
    ),
    "windows": BuildSettings(
        target="x86_64-pc-windows-msvc",
        strip=False,
        man_page=False,
        archive=Archive.ZIP,
        ext=".exe",
    ),
}[BUILD]

print(f"settings: {build}")


target_dir = pathlib.Path("target") / build.target / "release"
bins = [(target_dir / bin).with_suffix(build.ext) for bin in ["fwd", "fwd-browse"]]


def build_and_test(staging: pathlib.Path):
    # Tools
    subprocess.run(
        ["rustup", "target", "add", build.target],
        check=True,
    )

    # Test...?
    if build.test:
        subprocess.run(
            ["cargo", "test", "--verbose", "--release", "--target", build.target],
            check=True,
        )

    # Build
    subprocess.run(
        ["cargo", "build", "--verbose", "--release", "--target", build.target],
        check=True,
    )

    # Strip
    if build.strip:
        for bin in bins:
            subprocess.run(["strip", bin], check=True)

    # Copy
    for bin in bins:
        shutil.copyfile(bin, os.path.join(staging, os.path.basename(bin)))


def build_docs(staging: pathlib.Path):
    shutil.copyfile("README.md", staging / "README.md")
    if build.man_page:
        print("Creating man page...")
        proc = subprocess.run(
            ["pandoc", "-s", "-tman", os.path.join("doc", "fwd.man.md")],
            check=True,
            capture_output=True,
            encoding="utf8",
        )
        contents = proc.stdout
        with open(staging / "fwd.1", "w", encoding="utf-8") as f:
            f.write(contents)


def build_archive(staging: pathlib.Path) -> pathlib.Path:
    print("Creating archive...")
    if build.archive == Archive.ZIP:
        archive = pathlib.Path(f"{staging}.zip")
        subprocess.run(["7z", "a", archive, f"{staging}"], check=True)

    elif build.archive == Archive.DEB:
        subprocess.run(["cargo", "install", "cargo-deb"], check=True)
        shutil.copyfile(staging / "fwd.1", target_dir / "fwd.1")
        subprocess.run(["cargo", "deb", "--target", build.target], check=True)

        # Knowing the deb path means knowing the target version but I don't
        # actually have the version here. (Or, like, I have the release tag
        # but not in testing.) So just find the hopefully singular .deb that
        # we made.
        deb_path = pathlib.Path("target") / build.target / "debian"
        archives = list(deb_path.glob("*.deb"))
        assert len(archives) == 1
        archive = archives[0]

    else:
        assert build.archive == Archive.TARBALL
        archive = pathlib.Path(f"{staging}.tar.gz")
        subprocess.run(["tar", "czf", archive, f"{staging}"], check=True)

    return archive


staging = pathlib.Path(f"fwd-{build.target}")
os.makedirs(staging, exist_ok=True)

build_and_test(staging)
build_docs(staging)
archive = build_archive(staging)

shutil.rmtree(staging)

assert archive.exists()
if RELEASE_TAG is None:
    print(f"Not releasing {archive} to github, RELEASE_TAG is none.")
else:
    print(f"Uploading {archive} to github release {RELEASE_TAG}...")
    subprocess.run(
        ["gh", "release", "upload", RELEASE_TAG, archive, "--clobber"],
        check=True,
    )
    os.unlink(archive)
