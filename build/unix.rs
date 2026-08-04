pub fn configure() {
    super::detect_pkg_config_ffmpeg().unwrap_or_else(|| {
        panic!("Vivid media requires FFmpeg development libraries discoverable through pkg-config")
    });
}
