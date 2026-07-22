use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::Instant;

use pollster::block_on;
use vello::peniko::Color;
use vello::util::{RenderContext, RenderSurface};
use vello::{AaConfig, AaSupport, RenderParams, Renderer, RendererOptions, Scene, wgpu};
use winit::dpi::PhysicalSize;
use winit::window::Window as WinitWindow;

use crate::terminal::graphics::GraphicsCommand;

use crate::display::SizeInfo;
use crate::display::media::VividMediaRenderer;

#[derive(Debug)]
pub enum Error {
    CreateSurface(vello::Error),
    CreateRenderer(vello::Error),
    Render(vello::Error),
    SurfaceValidation,
}

/// Maximum raw screenshot readback allocation.
const MAX_SCREENSHOT_BYTES: u64 = 256 * 1024 * 1024;

/// Number of bytes in one captured RGBA pixel.
const SCREENSHOT_PIXEL_BYTES: u32 = 4;

#[derive(Debug)]
pub enum ScreenshotError {
    NoPresentedFrame,
    TooLarge,
    Device(String),
    Readback(String),
}

impl std::fmt::Display for ScreenshotError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoPresentedFrame => formatter.write_str("no displayed frame is available"),
            Self::TooLarge => formatter.write_str("screenshot exceeds the 256 MiB readback limit"),
            Self::Device(err) => write!(formatter, "screenshot device polling failed: {err}"),
            Self::Readback(err) => write!(formatter, "screenshot readback failed: {err}"),
        }
    }
}

impl std::error::Error for ScreenshotError {}

/// Asynchronous GPU screenshot readback.
pub struct ScreenshotReadback {
    receiver: Receiver<Result<Vec<u8>, String>>,
    pub width: u32,
    pub height: u32,
    pub padded_bytes_per_row: u32,
    pub started: Instant,
}

/// Completed screenshot pixels with WebGPU row padding still present.
pub struct ScreenshotPixels {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub padded_bytes_per_row: u32,
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
    render_target: wgpu::Texture,
    render_target_view: wgpu::TextureView,
    has_presented_frame: bool,
}

impl SceneRenderer {
    pub fn new(
        window: Arc<WinitWindow>,
        size: PhysicalSize<u32>,
        transparent: bool,
    ) -> Result<Self, Error> {
        let mut context = RenderContext::new();
        let size = clamp_render_size(size, wgpu::Limits::default().max_texture_dimension_2d);
        let valid_surface = size.width != 0 && size.height != 0;
        let mut surface = block_on(context.create_surface(
            window,
            size.width.max(1),
            size.height.max(1),
            wgpu::PresentMode::AutoVsync,
        ))
        .map_err(Error::CreateSurface)?;

        let alpha_modes = &surface
            .surface
            .get_capabilities(context.devices[surface.dev_id].adapter())
            .alpha_modes;
        surface.config.alpha_mode = surface_alpha_mode(alpha_modes);
        context.configure_surface(&surface);

        if surface.config.alpha_mode == wgpu::CompositeAlphaMode::PostMultiplied {
            log::info!("Surface alpha mode: {:?}", surface.config.alpha_mode);
        } else if transparent {
            log::warn!(
                "Window transparency is unavailable; the render surface does not support \
                 post-multiplied alpha (supported modes: {alpha_modes:?})"
            );
        }

        let max_surface_dimension =
            context.devices[surface.dev_id].device.limits().max_texture_dimension_2d.min(8192);
        let device = &context.devices[surface.dev_id].device;
        let media = VividMediaRenderer::new(device);
        let (render_target, render_target_view) =
            create_render_target(device, surface.config.width, surface.config.height);

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
            render_target,
            render_target_view,
            has_presented_frame: false,
        })
    }

    pub fn resize(&mut self, size: PhysicalSize<u32>) {
        let size = self.clamp_render_size(size);
        if size.width == 0 || size.height == 0 {
            self.valid_surface = false;
            self.has_presented_frame = false;
            return;
        }

        self.context.resize_surface(&mut self.surface, size.width, size.height);
        let device = &self.context.devices[self.surface.dev_id].device;
        (self.render_target, self.render_target_view) =
            create_render_target(device, size.width, size.height);
        self.valid_surface = true;
        self.has_presented_frame = false;
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

    pub fn render(&mut self, scene: &Scene, base_color: Color) -> Result<bool, Error> {
        if !self.valid_surface {
            return Ok(false);
        }

        let width = self.surface.config.width;
        let height = self.surface.config.height;
        let surface_texture = match self.surface.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(texture)
            | wgpu::CurrentSurfaceTexture::Suboptimal(texture) => texture,
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                return Ok(false);
            },
            wgpu::CurrentSurfaceTexture::Lost | wgpu::CurrentSurfaceTexture::Outdated => {
                self.context.resize_surface(&mut self.surface, width.max(1), height.max(1));
                return Ok(false);
            },
            wgpu::CurrentSurfaceTexture::Validation => return Err(Error::SurfaceValidation),
        };

        let device_handle = &self.context.devices[self.surface.dev_id];
        let renderer = self.renderers[self.surface.dev_id].as_mut().expect("renderer initialized");

        renderer
            .render_to_texture(
                &device_handle.device,
                &device_handle.queue,
                scene,
                &self.render_target_view,
                &RenderParams { base_color, width, height, antialiasing_method: AaConfig::Msaa8 },
            )
            .map_err(Error::Render)?;

        let surface_view =
            surface_texture.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder =
            device_handle.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("Vivido.vello.surface_blit"),
            });
        self.surface.blitter.copy(
            &device_handle.device,
            &mut encoder,
            &self.render_target_view,
            &surface_view,
        );
        device_handle.queue.submit([encoder.finish()]);
        surface_texture.present();
        self.has_presented_frame = true;
        let _ = device_handle.device.poll(wgpu::PollType::Poll);

        Ok(true)
    }

    /// Start asynchronously reading the last successfully presented frame.
    pub fn begin_screenshot(&self) -> Result<ScreenshotReadback, ScreenshotError> {
        if !self.has_presented_frame {
            return Err(ScreenshotError::NoPresentedFrame);
        }

        let width = self.surface.config.width;
        let height = self.surface.config.height;
        let (padded_bytes_per_row, buffer_size) = screenshot_layout(width, height)?;

        let device_handle = &self.context.devices[self.surface.dev_id];
        let readback = device_handle.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("vivido.screenshot.readback"),
            size: buffer_size,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder =
            device_handle.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("vivido.screenshot.copy"),
            });
        encoder.copy_texture_to_buffer(
            self.render_target.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_bytes_per_row),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        );
        device_handle.queue.submit([encoder.finish()]);

        let (sender, receiver) = mpsc::channel();
        let callback_buffer = readback.clone();
        readback.slice(..).map_async(wgpu::MapMode::Read, move |result| {
            let result = result
                .map_err(|err| err.to_string())
                .map(|()| callback_buffer.slice(..).get_mapped_range().to_vec());
            callback_buffer.unmap();
            let _ = sender.send(result);
        });

        Ok(ScreenshotReadback {
            receiver,
            width,
            height,
            padded_bytes_per_row,
            started: Instant::now(),
        })
    }

    /// Poll an asynchronous screenshot without blocking the renderer thread.
    pub fn poll_screenshot(
        &self,
        readback: &ScreenshotReadback,
    ) -> Result<Option<ScreenshotPixels>, ScreenshotError> {
        let device = &self.context.devices[self.surface.dev_id].device;
        device
            .poll(wgpu::PollType::Poll)
            .map_err(|err| ScreenshotError::Device(err.to_string()))?;

        match readback.receiver.try_recv() {
            Ok(Ok(bytes)) => Ok(Some(ScreenshotPixels {
                bytes,
                width: readback.width,
                height: readback.height,
                padded_bytes_per_row: readback.padded_bytes_per_row,
            })),
            Ok(Err(err)) => Err(ScreenshotError::Readback(err)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => {
                Err(ScreenshotError::Readback(String::from("readback channel disconnected")))
            },
        }
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

fn create_render_target(
    device: &wgpu::Device,
    width: u32,
    height: u32,
) -> (wgpu::Texture, wgpu::TextureView) {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("vivido.vello.render_target"),
        size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::STORAGE_BINDING
            | wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    (texture, view)
}

fn screenshot_layout(width: u32, height: u32) -> Result<(u32, u64), ScreenshotError> {
    let unpadded_bytes_per_row =
        width.checked_mul(SCREENSHOT_PIXEL_BYTES).ok_or(ScreenshotError::TooLarge)?;
    let alignment = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let padded_bytes_per_row = unpadded_bytes_per_row
        .checked_add(alignment - 1)
        .map(|bytes| bytes / alignment * alignment)
        .ok_or(ScreenshotError::TooLarge)?;
    let buffer_size = u64::from(padded_bytes_per_row)
        .checked_mul(u64::from(height))
        .filter(|size| *size <= MAX_SCREENSHOT_BYTES)
        .ok_or(ScreenshotError::TooLarge)?;
    Ok((padded_bytes_per_row, buffer_size))
}

fn surface_alpha_mode(alpha_modes: &[wgpu::CompositeAlphaMode]) -> wgpu::CompositeAlphaMode {
    let transparent_mode = wgpu::CompositeAlphaMode::PostMultiplied;
    if alpha_modes.contains(&transparent_mode) {
        transparent_mode
    } else {
        wgpu::CompositeAlphaMode::Auto
    }
}

#[cfg(test)]
mod tests {
    use super::{clamp_render_size, screenshot_layout, surface_alpha_mode};
    use vello::wgpu::CompositeAlphaMode;
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

    #[test]
    fn surface_prefers_transparency_over_opaque_auto_mode() {
        let modes = [CompositeAlphaMode::Opaque, CompositeAlphaMode::PostMultiplied];

        assert_eq!(surface_alpha_mode(&modes), CompositeAlphaMode::PostMultiplied);
    }

    #[test]
    fn surface_uses_auto_mode_without_straight_alpha_support() {
        let modes = [CompositeAlphaMode::Opaque, CompositeAlphaMode::PreMultiplied];

        assert_eq!(surface_alpha_mode(&modes), CompositeAlphaMode::Auto);
    }

    #[test]
    fn screenshot_layout_aligns_and_bounds_readback() {
        assert_eq!(screenshot_layout(1, 2).unwrap(), (256, 512));
        assert_eq!(screenshot_layout(8192, 8192).unwrap(), (32768, 256 * 1024 * 1024));
        assert!(screenshot_layout(u32::MAX, 1).is_err());
    }
}
