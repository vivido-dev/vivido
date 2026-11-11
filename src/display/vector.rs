//! Terminal font adapter for the shared SDK vector compiler.

// Rust guideline compliant 2026-10-07
use super::text::TextSystem;
use std::{collections::BTreeMap, sync::Arc};
use vello::peniko::ImageData;
use vivid_protocol::vector::{Canvas, InvalidScene};

use vivid_sdk::vector_renderer::parley;
pub use vivid_sdk::vector_renderer::{CompiledScene, text_layout};

impl vivid_sdk::vector_renderer::TextShaper for TextSystem {
    fn shape_overlay(&mut self, text: &vivid_protocol::vector::Text) -> parley::Layout<()> {
        TextSystem::shape_overlay(self, text)
    }
}

/// Compiles a complete portable scene with Vivido's native font selection.
///
/// # Errors
/// Returns an error for invalid geometry, assets or drawing operations.
pub fn compile(
    canvas: &Canvas,
    text: &mut TextSystem,
    images: &BTreeMap<u64, ImageData>,
) -> Result<CompiledScene, InvalidScene> {
    vivid_sdk::vector_renderer::compile(canvas, text, images)
}

/// Compiles a complete scene with retained, host-measured text layouts.
///
/// # Errors
/// Returns an error for invalid drawing commands or missing retained resources.
pub fn compile_with_layouts(
    canvas: &Canvas,
    text: &mut TextSystem,
    images: &BTreeMap<u64, ImageData>,
    layouts: &BTreeMap<u64, Arc<text_layout::MeasuredLayout>>,
) -> Result<CompiledScene, InvalidScene> {
    vivid_sdk::vector_renderer::compile_with_layouts(canvas, text, images, layouts)
}
