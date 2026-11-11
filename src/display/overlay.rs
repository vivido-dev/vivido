//! Shared SDK overlay composition with Vivido's scene acknowledgement lifecycle.

// Rust guideline compliant 2026-10-07
use crate::vivid::scene::SharedScene;
use vello::{Renderer, peniko::ImageData};
pub(crate) use vivid_sdk::vector_renderer::compositor::RenderMetrics as OverlayRenderMetrics;
use vivid_sdk::vector_renderer::compositor::{Compositor, Drawing};

#[derive(Default)]
pub(super) struct OverlayRenderer {
    pub scene: Option<SharedScene>,
    cache: Compositor,
    pub generation: u64,
}
impl OverlayRenderer {
    pub fn metrics(&self) -> OverlayRenderMetrics {
        self.cache.metrics()
    }
    pub fn finish(&mut self, presented: bool) {
        if let Some(scene) = &self.scene {
            scene.overlays().lock().unwrap_or_else(|p| p.into_inner()).finish(presented);
        }
    }
    pub fn clear(&mut self, renderer: &mut Renderer) {
        self.cache.clear(renderer);
        self.generation = self.cache.generation();
    }
    pub fn draw(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        renderer: &mut Renderer,
        width: u32,
        height: u32,
    ) -> Result<(Option<ImageData>, bool), vivid_sdk::vector_renderer::compositor::Error> {
        self.finish(false);
        let drawings: Vec<_> = self
            .scene
            .as_ref()
            .map(|scene| {
                scene
                    .overlays()
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .prepare(scene)
                    .into_iter()
                    .map(|drawing| Drawing {
                        window: drawing.window,
                        window_revision: drawing.window_revision,
                        bounds: drawing.bounds,
                        scale: drawing.scale,
                        compiled: drawing.compiled,
                        submission: drawing.submission,
                    })
                    .collect()
            })
            .unwrap_or_default();
        let result = self.cache.render(device, queue, renderer, &drawings, width, height);
        self.generation = self.cache.generation();
        result
    }
}
impl Drop for OverlayRenderer {
    fn drop(&mut self) {
        self.finish(false);
    }
}
