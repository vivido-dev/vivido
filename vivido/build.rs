use std::env;
#[cfg(windows)]
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let mut version = String::from(env!("CARGO_PKG_VERSION"));
    if let Some(commit_hash) = commit_hash() {
        version = format!("{version} ({commit_hash})");
    }
    println!("cargo:rustc-env=VERSION={version}");

    link_ffmpeg();

    #[cfg(windows)]
    embed_resource::compile("./windows/vivido.rc", embed_resource::NONE)
        .manifest_required()
        .unwrap();
}

fn link_ffmpeg() {
    for variable in
        ["PKG_CONFIG_PATH", "VCPKG_ROOT", "VCPKG_DEFAULT_TRIPLET", "VCPKG_TARGET_TRIPLET"]
    {
        println!("cargo:rerun-if-env-changed={variable}");
    }

    let libraries = ["libavcodec", "libavutil", "libswscale"];
    if libraries
        .iter()
        .all(|library| pkg_config::Config::new().cargo_metadata(false).probe(library).is_ok())
    {
        for library in libraries {
            pkg_config::Config::new()
                .cargo_metadata(true)
                .probe(library)
                .expect("FFmpeg pkg-config probe changed during the build");
        }
        return;
    }

    #[cfg(windows)]
    {
        link_vcpkg_ffmpeg();
        return;
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
    for library in ["avcodec", "avutil", "swscale"] {
        let import_library = library_directory.join(format!("{library}.lib"));
        assert!(
            import_library.is_file(),
            "Vivid media requires {}; install ffmpeg:{} with vcpkg",
            import_library.display(),
            triplet
        );
    }
    println!("cargo:rustc-link-search=native={}", library_directory.display());
    for library in ["avcodec", "avutil", "swscale"] {
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
