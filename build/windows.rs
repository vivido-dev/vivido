use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

pub fn configure() {
    for variable in [
        "VCPKG_ROOT",
        "VCPKG_DEFAULT_TRIPLET",
        "VCPKG_TARGET_TRIPLET",
        "ProgramFiles",
        "ProgramFiles(x86)",
    ] {
        println!("cargo:rerun-if-env-changed={variable}");
    }

    let runtime_dlls = if env::var_os("VCPKG_ROOT").is_some() {
        link_vcpkg_ffmpeg()
    } else if let Some(libraries) = super::detect_pkg_config_ffmpeg() {
        stage_pkg_config_ffmpeg_runtime(&libraries)
    } else {
        link_vcpkg_ffmpeg()
    };
    println!("cargo:ffmpeg_delay_load={}", runtime_dlls.join(","));
    configure_ffmpeg_delay_load(&runtime_dlls);
    stage_dxc_runtime();

    embed_resources();
}

fn embed_resources() {
    embed_resource::compile("./windows/vivido.rc", embed_resource::NONE)
        .manifest_required()
        .unwrap();
}

fn link_vcpkg_ffmpeg() -> Vec<String> {
    let root = env::var_os("VCPKG_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| panic!("Vivid media requires pkg-config or VCPKG_ROOT on Windows"));
    let triplet = env::var("VCPKG_TARGET_TRIPLET")
        .or_else(|_| env::var("VCPKG_DEFAULT_TRIPLET"))
        .unwrap_or_else(|_| default_triplet());
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
    stage_ffmpeg_runtime(&installed_directory.join("bin"))
}

fn stage_pkg_config_ffmpeg_runtime(libraries: &[pkg_config::Library]) -> Vec<String> {
    let runtime_directory = libraries
        .iter()
        .flat_map(|library| &library.link_paths)
        .filter_map(|path| path.parent().map(|parent| parent.join("bin")))
        .find(|path| path.is_dir())
        .unwrap_or_else(|| {
            panic!("pkg-config found FFmpeg import libraries but not its Windows runtime directory")
        });
    stage_ffmpeg_runtime(&runtime_directory)
}

/// Resolve FFmpeg only when a Vivid media track first needs it.
///
/// The MSVC loader otherwise maps FFmpeg, its codecs, and dav1d before `main`, adding substantial
/// cold-start I/O to every ordinary terminal launch. Delay imports retain the same linked ABI and
/// function calls while moving that work to the first media use. Keep the names tied to the DLLs
/// we actually staged instead of baking a particular FFmpeg major version into the build.
fn configure_ffmpeg_delay_load(runtime_dlls: &[String]) {
    if env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        return;
    }

    // Cargo applies binary link arguments to the binary's zero-test harness too. That harness
    // prunes all media calls, so link.exe would warn that every otherwise-valid delay import is
    // unused and turn a clean `cargo test`/clippy run noisy.
    println!("cargo:rustc-link-arg-bin=vivido=/IGNORE:4199");
    println!("cargo:rustc-link-arg-bin=vivido=delayimp.lib");
    for dll in runtime_dlls {
        println!("cargo:rustc-link-arg-bin=vivido=/DELAYLOAD:{dll}");
    }
}

/// Put the native runtime beside Cargo's executable outputs.
///
/// An MSVC import library is enough to link `vivido.exe`, but Windows does not search vcpkg's
/// `bin` directory when that executable is launched directly from `target\debug` or
/// `target\release`. Keep the app-local runtime in the active output directory so a successful
/// Cargo build is runnable without a developer-specific `PATH`.
fn stage_ffmpeg_runtime(runtime_directory: &Path) -> Vec<String> {
    let profile_directory = cargo_profile_directory();

    let entries = fs::read_dir(runtime_directory).unwrap_or_else(|error| {
        panic!(
            "Vivid media runtime directory {} is unavailable: {error}",
            runtime_directory.display()
        )
    });
    let mut staged_families = [false; 4];
    let mut runtime_dlls = Vec::with_capacity(staged_families.len());
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
            runtime_dlls.push(name.into_owned());
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
    runtime_dlls.sort_unstable();
    runtime_dlls
}

/// Stage the modern DirectX Shader Compiler beside `vivido.exe`.
///
/// wgpu otherwise falls back to legacy FXC, which turns Vello's one-time pipeline compilation into
/// a multi-second startup stall. Release builds get the pinned DXC from vcpkg; ordinary developer
/// builds may use a compatible Windows SDK copy. wgpu retains its FXC fallback at runtime if the
/// app-local DXC files cannot be loaded.
fn stage_dxc_runtime() {
    let runtime_directory = dxc_runtime_directory();
    let profile_directory = cargo_profile_directory();
    for name in ["dxcompiler.dll", "dxil.dll"] {
        let source = runtime_directory.join(name);
        println!("cargo:rerun-if-changed={}", source.display());
        let destination = profile_directory.join(name);
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
}

fn dxc_runtime_directory() -> PathBuf {
    if let Some(root) = env::var_os("VCPKG_ROOT") {
        let triplet = env::var("VCPKG_TARGET_TRIPLET")
            .or_else(|_| env::var("VCPKG_DEFAULT_TRIPLET"))
            .unwrap_or_else(|_| default_triplet());
        let directory = PathBuf::from(root).join("installed").join(triplet).join("bin");
        if has_dxc_runtime(&directory) {
            return directory;
        }
    }

    let program_files = env::var_os("ProgramFiles(x86)")
        .or_else(|| env::var_os("ProgramFiles"))
        .map(PathBuf::from)
        .unwrap_or_else(|| panic!("DirectX Shader Compiler requires ProgramFiles"));
    let sdk_bin = program_files.join("Windows Kits").join("10").join("bin");
    let architecture = match env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default().as_str() {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "x86",
        architecture => panic!("unsupported Windows target architecture {architecture:?}"),
    };
    // wgpu 29 requires DXC 1.8.2502, first shipped in Windows SDK 10.0.26100.0. Staging an older
    // SDK DLL is worse than omitting it because it can load successfully but lack required APIs.
    const MINIMUM_DXC_SDK: [u32; 4] = [10, 0, 26100, 0];
    let mut versions = fs::read_dir(&sdk_bin)
        .unwrap_or_else(|error| {
            panic!("could not inspect Windows SDK {}: {error}", sdk_bin.display())
        })
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter(|entry| {
            windows_sdk_version(&entry.file_name()).is_some_and(|v| v >= MINIMUM_DXC_SDK)
        })
        .map(|entry| entry.path().join(architecture))
        .filter(|directory| has_dxc_runtime(directory))
        .collect::<Vec<_>>();
    versions.sort_unstable();
    versions.pop().unwrap_or_else(|| {
        panic!(
            "DirectX Shader Compiler was not found in vcpkg or the Windows SDK; install the \
             directx-dxc vcpkg package"
        )
    })
}

fn windows_sdk_version(name: &OsStr) -> Option<[u32; 4]> {
    let mut parts = name.to_str()?.split('.').map(str::parse::<u32>);
    let version =
        [parts.next()?.ok()?, parts.next()?.ok()?, parts.next()?.ok()?, parts.next()?.ok()?];
    parts.next().is_none().then_some(version)
}

fn has_dxc_runtime(directory: &Path) -> bool {
    ["dxcompiler.dll", "dxil.dll"].iter().all(|name| directory.join(name).is_file())
}

fn cargo_profile_directory() -> PathBuf {
    let out_directory = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo did not set OUT_DIR"));
    out_directory
        .ancestors()
        .nth(3)
        .filter(|path| path.join("build").is_dir())
        .unwrap_or_else(|| panic!("unexpected Cargo OUT_DIR layout: {}", out_directory.display()))
        .to_owned()
}

fn files_are_equal(left: &Path, right: &Path) -> bool {
    let Ok(left_metadata) = fs::metadata(left) else { return false };
    let Ok(right_metadata) = fs::metadata(right) else { return false };
    if left_metadata.len() != right_metadata.len() {
        return false;
    }

    fs::read(left).ok().zip(fs::read(right).ok()).is_some_and(|(left, right)| left == right)
}

fn default_triplet() -> String {
    let architecture = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    match architecture.as_str() {
        "x86_64" => "x64-windows",
        "aarch64" => "arm64-windows",
        "x86" => "x86-windows",
        _ => panic!("unsupported Windows target architecture {architecture:?}"),
    }
    .to_owned()
}
