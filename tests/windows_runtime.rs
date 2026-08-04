#![cfg(windows)]

use std::fs;
use std::path::Path;

#[test]
fn cargo_build_stages_ffmpeg_runtime_beside_vivido() {
    let binary = Path::new(env!("CARGO_BIN_EXE_vivido"));
    let output_directory = binary.parent().expect("Vivido binary has no parent directory");
    let names: Vec<_> = fs::read_dir(output_directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();

    for family in ["avcodec-", "avutil-", "swscale-", "swresample-"] {
        assert!(
            names.iter().any(|name| name.starts_with(family) && name.ends_with(".dll")),
            "{family}*.dll was not staged beside {}",
            binary.display()
        );
    }
    assert!(
        names.iter().any(|name| name.eq_ignore_ascii_case("dav1d.dll")),
        "dav1d.dll was not staged beside {}",
        binary.display()
    );
}
