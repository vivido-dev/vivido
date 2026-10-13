//! Keep published parser dependencies compatible with Vivido's terminal handler.

// Rust guideline compliant 2026-10-07

use semver::{Version, VersionReq};

#[test]
fn published_parser_dependency_requires_apc_and_cell_size_callbacks() {
    let manifest: toml::Value = toml::from_str(include_str!("../Cargo.toml")).unwrap();
    let requirement = manifest["dependencies"]["vvte"]["version"].as_str().unwrap();
    let requirement = VersionReq::parse(requirement).unwrap();

    // Packaging removes the local path, so older registry releases must be rejected.
    for patch in 0..2 {
        let version = Version::new(0, 15, patch);
        assert!(!requirement.matches(&version), "vvte {version} lacks the required callbacks");
    }
    assert!(requirement.matches(&Version::new(0, 15, 2)));
}
