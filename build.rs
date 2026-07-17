use std::env;
use std::path::Path;
#[cfg(windows)]
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let mut version = String::from(env!("CARGO_PKG_VERSION"));
    if let Some(commit_hash) = commit_hash() {
        version = format!("{version} ({commit_hash})");
    }
    println!("cargo:rustc-env=VERSION={version}");

    configure_ref_tests();
    link_ffmpeg();

    #[cfg(windows)]
    embed_resource::compile("./windows/vivido.rc", embed_resource::NONE)
        .manifest_required()
        .unwrap();
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
        link_vcpkg_ffmpeg();
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
        return;
    }

    #[cfg(windows)]
    {
        link_vcpkg_ffmpeg();
    }

    #[cfg(not(windows))]
    panic!("Vivid media requires FFmpeg development libraries discoverable through pkg-config");
}

#[cfg(windows)]
fn link_vcpkg_ffmpeg() {
    let root = env::var_os("VCPKG_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| panic!("Vivid media requires pkg-config or VCPKG_ROOT on Windows"));
    let triplet = env::var("VCPKG_TARGET_TRIPLET")
        .or_else(|_| env::var("VCPKG_DEFAULT_TRIPLET"))
        .unwrap_or_else(|_| default_windows_triplet());
    let library_directory = root.join("installed").join(&triplet).join("lib");
    for library in ["avcodec", "avutil", "swscale", "swresample"] {
        let import_library = library_directory.join(format!("{library}.lib"));
        assert!(
            import_library.is_file(),
            "Vivid media requires {}; install ffmpeg:{} with vcpkg",
            import_library.display(),
            triplet
        );
    }
    println!("cargo:rustc-link-search=native={}", library_directory.display());
    for library in ["avcodec", "avutil", "swscale", "swresample"] {
        println!("cargo:rustc-link-lib=dylib={library}");
    }
}

#[cfg(windows)]
fn default_windows_triplet() -> String {
    let architecture = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    match architecture.as_str() {
        "x86_64" => "x64-windows",
        "aarch64" => "arm64-windows",
        "x86" => "x86-windows",
        _ => panic!("unsupported Windows target architecture {architecture:?}"),
    }
    .to_owned()
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
