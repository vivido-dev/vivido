use std::sync::Arc;

use pollster::block_on;
use vello::peniko::Color;
use vello::util::{RenderContext, RenderSurface};
use vello::{AaConfig, AaSupport, RenderParams, Renderer, RendererOptions, Scene, wgpu};
use winit::dpi::PhysicalSize;
use winit::window::Window as WinitWindow;

use vivido_terminal::graphics::GraphicsCommand;

use crate::display::SizeInfo;
use crate::display::media::VividMediaRenderer;

#[derive(Debug)]
pub enum Error {
    CreateSurface(vello::Error),
    CreateRenderer(vello::Error),
    Render(vello::Error),
    SurfaceValidation,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CreateSurface(err) => write!(f, "failed to create render surface: {err}"),
            Self::CreateRenderer(err) => write!(f, "failed to create renderer: {err}"),
            Self::Render(err) => write!(f, "failed to render scene: {err}"),
            Self::SurfaceValidation => write!(f, "surface texture validation failed"),
        }
    }
}

impl std::error::Error for Error {}

pub struct SceneRenderer {
    context: RenderContext,
    renderers: Vec<Option<Renderer>>,
    surface: Box<RenderSurface<'static>>,
    valid_surface: bool,
    max_surface_dimension: u32,
    media: VividMediaRenderer,
}

impl SceneRenderer {
    pub fn new(window: Arc<WinitWindow>, size: PhysicalSize<u32>) -> Result<Self, Error> {
        let mut context = RenderContext::new();
        let size = clamp_render_size(size, wgpu::Limits::default().max_texture_dimension_2d);
        let valid_surface = size.width != 0 && size.height != 0;
        let surface = block_on(context.create_surface(
            window,
            size.width.max(1),
            size.height.max(1),
            wgpu::PresentMode::AutoVsync,
        ))
        .map_err(Error::CreateSurface)?;

        let max_surface_dimension =
            context.devices[surface.dev_id].device.limits().max_texture_dimension_2d.min(8192);
        let media = VividMediaRenderer::new(&context.devices[surface.dev_id].device);

        let mut renderers = Vec::new();
        renderers.resize_with(context.devices.len(), || None);
        renderers[surface.dev_id] = Some(
            Renderer::new(
                &context.devices[surface.dev_id].device,
                RendererOptions {
                    antialiasing_support: [AaConfig::Msaa8].into_iter().collect::<AaSupport>(),
                    ..RendererOptions::default()
                },
            )
            .map_err(Error::CreateRenderer)?,
        );

        Ok(Self {
            context,
            renderers,
            surface: Box::new(surface),
            valid_surface,
            max_surface_dimension,
            media,
        })
    }

    pub fn resize(&mut self, size: PhysicalSize<u32>) {
        let size = self.clamp_render_size(size);
        if size.width == 0 || size.height == 0 {
            self.valid_surface = false;
            return;
        }

        self.context.resize_surface(&mut self.surface, size.width, size.height);
        self.valid_surface = true;
    }

    pub fn clamp_render_size(&self, size: PhysicalSize<u32>) -> PhysicalSize<u32> {
        clamp_render_size(size, self.max_surface_dimension)
    }

    pub fn set_vivid_scene(&mut self, scene: crate::vivid::scene::SharedScene) {
        self.media.set_scene(scene);
    }

    pub fn submit_graphics(&mut self, command: GraphicsCommand) {
        self.media.submit(command);
    }

    pub fn prepare_media(
        &mut self,
        size: &SizeInfo,
        display_offset: usize,
    ) -> Option<vello::peniko::ImageData> {
        let device_handle = &self.context.devices[self.surface.dev_id];
        let renderer = self.renderers[self.surface.dev_id].as_mut().expect("renderer initialized");
        self.media.draw(&device_handle.device, &device_handle.queue, renderer, size, display_offset)
    }

    pub fn render(&mut self, scene: &Scene, base_color: Color) -> Result<(), Error> {
        if !self.valid_surface {
            return Ok(());
        }

        let width = self.surface.config.width;
        let height = self.surface.config.height;
        let device_handle = &self.context.devices[self.surface.dev_id];
        let renderer = self.renderers[self.surface.dev_id].as_mut().expect("renderer initialized");

        renderer
            .render_to_texture(
                &device_handle.device,
                &device_handle.queue,
                scene,
                &self.surface.target_view,
                &RenderParams { base_color, width, height, antialiasing_method: AaConfig::Msaa8 },
            )
            .map_err(Error::Render)?;

        let surface_texture = match self.surface.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(texture)
            | wgpu::CurrentSurfaceTexture::Suboptimal(texture) => texture,
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                return Ok(());
            },
            wgpu::CurrentSurfaceTexture::Lost | wgpu::CurrentSurfaceTexture::Outdated => {
                self.context.resize_surface(&mut self.surface, width.max(1), height.max(1));
                return Ok(());
            },
            wgpu::CurrentSurfaceTexture::Validation => return Err(Error::SurfaceValidation),
        };

        let surface_view =
            surface_texture.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder =
            device_handle.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("Vivido.vello.surface_blit"),
            });
        self.surface.blitter.copy(
            &device_handle.device,
            &mut encoder,
            &self.surface.target_view,
            &surface_view,
        );
        device_handle.queue.submit([encoder.finish()]);
        surface_texture.present();
        let _ = device_handle.device.poll(wgpu::PollType::Poll);

        Ok(())
    }
}

impl Drop for SceneRenderer {
    fn drop(&mut self) {
        if let Some(renderer) = self.renderers[self.surface.dev_id].as_mut() {
            self.media.clear_target(renderer);
        }
    }
}

fn clamp_render_size(size: PhysicalSize<u32>, max_dimension: u32) -> PhysicalSize<u32> {
    PhysicalSize::new(size.width.min(max_dimension), size.height.min(max_dimension))
}

#[cfg(test)]
mod tests {
    use super::clamp_render_size;
    use winit::dpi::PhysicalSize;

    #[test]
    fn render_size_preserves_zero_and_clamps_each_axis() {
        assert_eq!(
            clamp_render_size(PhysicalSize::new(0, 10_000), 8192),
            PhysicalSize::new(0, 8192)
        );
        assert_eq!(
            clamp_render_size(PhysicalSize::new(640, 480), 8192),
            PhysicalSize::new(640, 480)
        );
    }
}
