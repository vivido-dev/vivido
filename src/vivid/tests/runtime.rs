//! Resolve the binding fixture's Node runtime without selecting a broken installation.

// Rust guideline compliant 2026-10-07
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{Command, Stdio};

pub(super) fn node() -> OsString {
    // An explicit selection stays authoritative, including its startup errors.
    if let Some(node) = std::env::var_os("VIVID_OVERLAY_TEST_NODE") {
        return node;
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    let executable = format!("node{}", std::env::consts::EXE_SUFFIX);
    let candidates = std::env::split_paths(&path).map(|directory| directory.join(&executable));
    runnable(candidates).expect("no working Node on PATH; set VIVID_OVERLAY_TEST_NODE").into()
}

fn runnable(candidates: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    candidates.into_iter().find(|candidate| {
        if !candidate.is_file() {
            return false;
        }
        // A binary may exist but fail to load a shared library. Probe before
        // starting the authenticated producer or its presentation deadline.
        let ready = Command::new(candidate)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        if !ready {
            eprintln!("unusable Node runtime: {}", candidate.display());
        }
        ready
    })
}

#[cfg(unix)]
#[test]
fn broken_runtime_does_not_shadow_a_working_later_installation() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let broken = directory.path().join("broken-node");
    let working = directory.path().join("working-node");
    for (path, script) in
        [(&broken, "#!/bin/sh\nexit 127\n"), (&working, "#!/bin/sh\n[ \"$1\" = --version ]\n")]
    {
        std::fs::write(path, script).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    assert_eq!(runnable([directory.path().join("missing-node"), broken.clone()]), None);
    assert_eq!(runnable([broken, working.clone()]), Some(working));
}
