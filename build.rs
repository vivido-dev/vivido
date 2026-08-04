use std::env;
#[cfg(windows)]
use std::fs;
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
        #[cfg(windows)]
        stage_pkg_config_ffmpeg_runtime(&detected);
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
    let installed_directory = root.join("installed").join(&triplet);
    let library_directory = installed_directory.join("lib");
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
    stage_ffmpeg_runtime(&installed_directory.join("bin"));
}

/// Put the native runtime beside Cargo's executable outputs.
///
/// An MSVC import library is enough to link `vivido.exe`, but Windows does not search vcpkg's
/// `bin` directory when that executable is launched directly from `target\debug` or
/// `target\release`. Keep the app-local runtime in both output directories so a successful Cargo
/// build is runnable without a developer-specific `PATH`.
#[cfg(windows)]
fn stage_ffmpeg_runtime(runtime_directory: &Path) {
    let out_directory = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo did not set OUT_DIR"));
    let profile_directory =
        out_directory.ancestors().nth(3).filter(|path| path.join("build").is_dir()).unwrap_or_else(
            || panic!("unexpected Cargo OUT_DIR layout: {}", out_directory.display()),
        );

    let entries = fs::read_dir(runtime_directory).unwrap_or_else(|error| {
        panic!(
            "Vivid media runtime directory {} is unavailable: {error}",
            runtime_directory.display()
        )
    });
    let mut staged_families = [false; 4];
    for entry in entries {
        let entry = entry.unwrap_or_else(|error| {
            panic!("could not inspect {}: {error}", runtime_directory.display())
        });
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();
        let family = ["avcodec-", "avutil-", "swscale-", "swresample-"]
            .iter()
            .position(|prefix| name.starts_with(prefix) && name.ends_with(".dll"));
        if let Some(index) = family {
            staged_families[index] = true;
        } else if !name.eq_ignore_ascii_case("dav1d.dll") {
            continue;
        }

        let source = entry.path();
        println!("cargo:rerun-if-changed={}", source.display());
        let destination = profile_directory.join(&file_name);
        if !files_are_equal(&source, &destination) {
            fs::copy(&source, &destination).unwrap_or_else(|error| {
                panic!(
                    "could not stage {} beside Cargo executables at {}: {error}",
                    source.display(),
                    destination.display()
                )
            });
        }
    }

    for (present, family) in staged_families.into_iter().zip([
        "avcodec-*.dll",
        "avutil-*.dll",
        "swscale-*.dll",
        "swresample-*.dll",
    ]) {
        assert!(present, "Vivid media requires {family} in {}", runtime_directory.display());
    }
}

#[cfg(windows)]
fn files_are_equal(left: &Path, right: &Path) -> bool {
    let Ok(left_metadata) = fs::metadata(left) else { return false };
    let Ok(right_metadata) = fs::metadata(right) else { return false };
    if left_metadata.len() != right_metadata.len() {
        return false;
    }

    fs::read(left).ok().zip(fs::read(right).ok()).is_some_and(|(left, right)| left == right)
}

#[cfg(windows)]
fn stage_pkg_config_ffmpeg_runtime(libraries: &[pkg_config::Library]) {
    let runtime_directory = libraries
        .iter()
        .flat_map(|library| &library.link_paths)
        .filter_map(|path| path.parent().map(|parent| parent.join("bin")))
        .find(|path| path.is_dir())
        .unwrap_or_else(|| {
            panic!("pkg-config found FFmpeg import libraries but not its Windows runtime directory")
        });
    stage_ffmpeg_runtime(&runtime_directory);
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
