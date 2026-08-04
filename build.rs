use std::env;
use std::path::Path;
use std::process::Command;

#[cfg(windows)]
mod windows;

fn main() {
    let mut version = String::from(env!("CARGO_PKG_VERSION"));
    if let Some(commit_hash) = commit_hash() {
        version = format!("{version} ({commit_hash})");
    }
    println!("cargo:rustc-env=VERSION={version}");

    configure_ref_tests();
    link_ffmpeg();

    #[cfg(windows)]
    windows::embed_resources();
}

fn configure_ref_tests() {
    println!("cargo:rustc-check-cfg=cfg(vivido_ref_tests)");
    println!("cargo:rerun-if-changed=tests/ref");
    if Path::new("tests/ref").is_dir() {
        println!("cargo:rustc-cfg=vivido_ref_tests");
    }
}

fn link_ffmpeg() {
    for variable in
        ["PKG_CONFIG_PATH", "VCPKG_ROOT", "VCPKG_DEFAULT_TRIPLET", "VCPKG_TARGET_TRIPLET"]
    {
        println!("cargo:rerun-if-env-changed={variable}");
    }

    #[cfg(windows)]
    if env::var_os("VCPKG_ROOT").is_some() {
        windows::link_vcpkg_ffmpeg();
        return;
    }

    let libraries = ["libavcodec", "libavutil", "libswscale", "libswresample"];
    let detected = libraries
        .iter()
        .map(|library| pkg_config::Config::new().cargo_metadata(false).probe(library))
        .collect::<Result<Vec<_>, _>>();
    if let Ok(detected) = detected {
        // Native library declarations live beside the FFI definitions so they are attached to the
        // executable, not this package's proc-macro library. The build script only supplies the
        // implementation-specific search paths discovered through pkg-config.
        for path in detected.iter().flat_map(|library| &library.link_paths) {
            println!("cargo:rustc-link-search=native={}", path.display());
        }
        #[cfg(windows)]
        windows::stage_pkg_config_ffmpeg_runtime(&detected);
        return;
    }

    #[cfg(windows)]
    {
        windows::link_vcpkg_ffmpeg();
    }

    #[cfg(not(windows))]
    panic!("Vivid media requires FFmpeg development libraries discoverable through pkg-config");
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
