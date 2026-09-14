//! Cached pane overlay target. Scene compilation and text shaping happen on channel workers.

use crate::vivid::overlay::Drawing;
use crate::vivid::scene::SharedScene;
use vello::kurbo::{Affine, Rect};
use vello::peniko::{Color, Fill, ImageAlphaType, ImageData, Mix};
use vello::{AaConfig, RenderParams, Renderer, Scene, wgpu};

#[derive(Default)]
pub(super) struct OverlayRenderer {
    pub scene: Option<SharedScene>,
    target: Option<Target>,
    key: Vec<(
        vivid_protocol::identity::SurfaceIdentity,
        u64,
        u64,
        vivid_protocol::vector::Rect,
        u64,
    )>,
    pub generation: u64,
}
struct Target {
    _texture: wgpu::Texture,
    view: wgpu::TextureView,
    image: ImageData,
    width: u32,
    height: u32,
}
impl OverlayRenderer {
    pub fn clear(&mut self, renderer: &mut Renderer) {
        if let Some(target) = self.target.take() {
            renderer.unregister_texture(target.image);
        }
        self.key.clear();
        self.generation = self.generation.wrapping_add(1);
    }

    pub fn draw(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        renderer: &mut Renderer,
        width: u32,
        height: u32,
    ) -> Result<(Option<ImageData>, bool), vello::Error> {
        let drawings = self
            .scene
            .as_ref()
            .map(|scene| scene.overlays().lock().unwrap_or_else(|p| p.into_inner()).drawing(scene))
            .unwrap_or_default();
        if drawings.is_empty() && self.target.is_none() {
            return Ok((None, false));
        }
        let width = width.clamp(1, device.limits().max_texture_dimension_2d);
        let height = height.clamp(1, device.limits().max_texture_dimension_2d);
        let new_target =
            self.target.as_ref().is_none_or(|t| t.width != width || t.height != height);
        if new_target {
            self.clear(renderer);
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("vivido.overlay.target"),
                size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let view = texture.create_view(&Default::default());
            let mut image = renderer.register_texture(texture.clone());
            image.alpha_type = ImageAlphaType::AlphaPremultiplied;
            self.target = Some(Target { _texture: texture, view, image, width, height });
        }
        let key: Vec<_> = drawings
            .iter()
            .map(|d| (d.window, d.window_revision, d.revision, d.bounds, d.scale.to_bits()))
            .collect();
        let target = self.target.as_ref().expect("overlay target allocated");
        if new_target || self.key != key {
            let scene = compose(&drawings, width, height);
            renderer.render_to_texture(
                device,
                queue,
                &scene,
                &target.view,
                &RenderParams {
                    base_color: Color::TRANSPARENT,
                    width,
                    height,
                    antialiasing_method: AaConfig::Msaa8,
                },
            )?;
            renderer.mark_override_image_dirty(&target.image);
            self.key = key;
            Ok((Some(target.image.clone()), true))
        } else {
            Ok((Some(target.image.clone()), false))
        }
    }
}

fn compose(drawings: &[Drawing], width: u32, height: u32) -> Scene {
    let mut scene = Scene::new();
    scene.push_layer(
        Fill::NonZero,
        Mix::Normal,
        1.,
        Affine::IDENTITY,
        &Rect::new(0., 0., f64::from(width), f64::from(height)),
    );
    for drawing in drawings {
        let b = drawing.bounds;
        let transform =
            Affine::scale(drawing.scale) * Affine::translate((b.origin.x.get(), b.origin.y.get()));
        scene.push_layer(
            Fill::NonZero,
            Mix::Normal,
            1.,
            transform,
            &Rect::new(0., 0., b.width.get(), b.height.get()),
        );
        scene.append(&drawing.compiled.scene, Some(transform));
        scene.pop_layer();
    }
    scene.pop_layer();
    scene
}
