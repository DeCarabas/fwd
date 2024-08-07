use std::io::Write;
use std::path::{absolute, Path, PathBuf};

/// Fetch the contents of the given file, and also tell cargo that we looked
/// in there.
fn file_contents<P: AsRef<Path>>(path: P) -> String {
    let path =
        absolute(path.as_ref()).expect("Unable to make the path absolute");

    let mut stdout = std::io::stdout();
    stdout
        .write_all(b"cargo::rerun-if-changed=")
        .expect("Unable to write stdout");
    stdout
        .write_all(path.as_os_str().as_encoded_bytes())
        .expect("Unable to write path to stdout");
    stdout
        .write_all(b"\n")
        .expect("Unable to write newline to stdout");

    std::fs::read_to_string(path).expect("Unable to read file")
}

/// Emit the current git commit.
fn emit_git_commit() {
    // Fetch the current commit from the head. We do it this way instead of
    // asking `git rev-parse` to do it for us because we want to reliably
    // tell cargo which files it should monitor for changes.
    let head = file_contents("./.git/HEAD");
    let rev = if let Some(r) = head.strip_prefix("ref: ") {
        let mut ref_path = PathBuf::from("./.git/");
        ref_path.push(r.trim());
        file_contents(ref_path)
    } else {
        head
    };

    // But *now* we ask git rev-parse to make this into a short hash (a) to
    // make sure we got it right and (b) because git knows how to quickly
    // determine how much of a commit is required to be unique. We don't need
    // to tell cargo anything here, no file that git consults will be
    // mutable.
    let output = std::process::Command::new("git")
        .arg("rev-parse")
        .arg("--short")
        .arg(rev.trim())
        .output()
        .expect("could not spawn `git` to get the hash");
    if !output.status.success() {
        let stderr = std::str::from_utf8(&output.stderr)
            .expect("git failed and stderr was not utf8");
        eprintln!("`git rev-parse --short HEAD` failed, stderr: {stderr}");
        panic!("`git rev-parse --short HEAD` failed");
    }
    let rev = std::str::from_utf8(&output.stdout)
        .expect("git did not output utf8")
        .trim();

    println!("cargo::rustc-env=REPO_REV={rev}");
}

fn emit_git_dirty() {
    // Here is the way to see if anything is up with the repository: run `git
    // status --porcelain=v1`. The status output in the v1 porcelain format
    // has one line for every file that's modified in some way: staged,
    // changed but unstaged, untracked, you name it. Files in the working
    // tree that are up to date with the repository are not emitted. This is
    // exactly what we want.
    //
    // (Yes, I want to track untracked files, because they can mess with the
    // build too. The only good build is a clean build!)
    let output = std::process::Command::new("git")
        .arg("status")
        .arg("-z")
        .arg("--porcelain=v1")
        .output()
        .expect("could not spawn `git` to get repository status");
    if !output.status.success() {
        let stderr = std::str::from_utf8(&output.stderr)
            .expect("git failed and stderr was not utf8");
        eprintln!("`git status` failed, stderr: {stderr}");
        panic!("`git status` failed");
    }
    let output =
        std::str::from_utf8(&output.stdout).expect("git did not output utf8");

    // If there *was* any output, parse it and tell cargo to re-run if any of
    // these files changed. (Maybe they get reverted! Then the repo status
    // will change.)
    let mut split = output.split('\x00');
    while let Some(field) = split.next() {
        if field.is_empty() {
            continue;
        }
        let prefix = &field[0..3];
        println!("cargo::rerun-if-changed={}", &field[3..]);
        let b = prefix.as_bytes();
        if b[0] == b'R' || b[1] == b'R' || b[0] == b'C' || b[1] == b'C' {
            if let Some(additional) = split.next() {
                println!("cargo::rerun-if-changed={additional}");
            }
        }
    }
    // Emit the repository status.
    let dirty = if output.trim().is_empty() {
        ""
    } else {
        " *dirty*"
    };
    println!("cargo::rustc-env=REPO_DIRTY={dirty}");
}

fn main() {
    emit_git_commit();
    emit_git_dirty();
}
