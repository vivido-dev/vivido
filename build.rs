use std::env;
use std::path::Path;
use std::process::Command;

#[cfg(windows)]
#[path = "build/windows.rs"]
mod platform;
#[cfg(unix)]
#[path = "build/unix.rs"]
mod platform;

fn main() {
    let mut version = String::from(env!("CARGO_PKG_VERSION"));
    if let Some(commit_hash) = commit_hash() {
        version = format!("{version} ({commit_hash})");
    }
    println!("cargo:rustc-env=VERSION={version}");

    configure_ref_tests();
    platform::configure();
}

fn configure_ref_tests() {
    println!("cargo:rustc-check-cfg=cfg(vivido_ref_tests)");
    println!("cargo:rerun-if-changed=tests/ref");
    if Path::new("tests/ref").is_dir() {
        println!("cargo:rustc-cfg=vivido_ref_tests");
    }
}

/// Discover the platform FFmpeg implementation without declaring libraries for the build script.
fn detect_pkg_config_ffmpeg() -> Option<Vec<pkg_config::Library>> {
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_PATH");
    let libraries = ["libavcodec", "libavutil", "libswscale", "libswresample"];
    let detected = libraries
        .iter()
        .map(|library| pkg_config::Config::new().cargo_metadata(false).probe(library))
        .collect::<Result<Vec<_>, _>>()
        .ok()?;

    // Native library declarations live beside the FFI definitions so they are attached to the
    // executable, not this package's proc-macro library. The build script only supplies the
    // implementation-specific search paths discovered through pkg-config.
    for path in detected.iter().flat_map(|library| &library.link_paths) {
        println!("cargo:rustc-link-search=native={}", path.display());
    }
    Some(detected)
}

fn commit_hash() -> Option<String> {
    Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|hash| hash.trim().into())
}
