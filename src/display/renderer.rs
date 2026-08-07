use std::cell::RefCell;
use std::rc::Rc;
#[cfg(any(unix, windows))]
use std::sync::mpsc::{self, Receiver, TryRecvError};
#[cfg(any(unix, windows))]
use std::time::Instant;

use pollster::block_on;
use vello::peniko::Color;
use vello::util::{RenderContext, RenderSurface};
use vello::{AaConfig, AaSupport, RenderParams, Renderer, RendererOptions, Scene, wgpu};
use winit::dpi::{PhysicalPosition, PhysicalSize};

use crate::terminal::graphics::GraphicsCommand;

use crate::display::SizeInfo;
#[cfg(any(unix, windows))]
use crate::display::media::CaptureRedaction;
use crate::display::media::VividMediaRenderer;
use crate::display::window::RenderSource;

#[derive(Debug)]
pub enum Error {
    CreateSurface(vello::Error),
    CreateRenderer(vello::Error),
    Render(vello::Error),
    SurfaceValidation,
    NoOffscreenAdapter(String),
    NoWindowDevice,
}

/// Maximum raw screenshot readback allocation.
#[cfg(any(unix, windows, test))]
const MAX_SCREENSHOT_BYTES: u64 = 256 * 1024 * 1024;

/// Number of bytes in one captured RGBA pixel.
#[cfg(any(unix, windows, test))]
const SCREENSHOT_PIXEL_BYTES: u32 = 4;

#[cfg(any(unix, windows, test))]
#[derive(Debug)]
pub enum ScreenshotError {
    #[cfg(any(unix, windows))]
    NoPresentedFrame,
    TooLarge,
    #[cfg(any(unix, windows))]
    Device(String),
    #[cfg(any(unix, windows))]
    Readback(String),
}

#[cfg(any(unix, windows, test))]
impl std::fmt::Display for ScreenshotError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(any(unix, windows))]
            Self::NoPresentedFrame => formatter.write_str("no displayed frame is available"),
            Self::TooLarge => formatter.write_str("screenshot exceeds the 256 MiB readback limit"),
            #[cfg(any(unix, windows))]
            Self::Device(err) => write!(formatter, "screenshot device polling failed: {err}"),
            #[cfg(any(unix, windows))]
            Self::Readback(err) => write!(formatter, "screenshot readback failed: {err}"),
        }
    }
}

#[cfg(any(unix, windows, test))]
impl std::error::Error for ScreenshotError {}

/// Asynchronous GPU screenshot readback.
#[cfg(any(unix, windows))]
pub struct ScreenshotReadback {
    receiver: Receiver<Result<Vec<u8>, String>>,
    pub width: u32,
    pub height: u32,
    pub padded_bytes_per_row: u32,
    pub started: Instant,
    pub redactions: Vec<CaptureRedaction>,
}

/// Completed screenshot pixels with WebGPU row padding still present.
#[cfg(any(unix, windows))]
pub struct ScreenshotPixels {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub padded_bytes_per_row: u32,
    pub redactions: Vec<CaptureRedaction>,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CreateSurface(err) => write!(f, "failed to create render surface: {err}"),
            Self::CreateRenderer(err) => write!(f, "failed to create renderer: {err}"),
            Self::Render(err) => write!(f, "failed to render scene: {err}"),
            Self::SurfaceValidation => write!(f, "surface texture validation failed"),
            Self::NoOffscreenAdapter(err) => write!(f, "{err}"),
            Self::NoWindowDevice => {
                f.write_str("an embedded renderer requires an initialized window renderer")
            },
        }
    }
}

impl std::error::Error for Error {}

pub struct SceneRenderer {
    /// Owns the wgpu instance and the surface's device. Absent when rendering offscreen.
    context: Option<SharedRenderContext>,
    /// Absent when rendering offscreen: there is no windowing-system surface to present to.
    surface: Option<Box<RenderSurface<'static>>>,
    renderer: Rc<RefCell<Renderer>>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    /// Whether the render target has a drawable (non-zero) size.
    valid_target: bool,
    max_surface_dimension: u32,
    media: VividMediaRenderer,
    render_target: wgpu::Texture,
    render_target_view: wgpu::TextureView,
    /// Size of `render_target`, which is also the surface size when there is a surface.
    target_size: PhysicalSize<u32>,
    /// Whether `render_target` holds a frame. Screenshots read that texture, not the swapchain,
    /// so this is set by both the windowed and the offscreen path.
    has_rendered_frame: bool,
}

/// Latest GPU frame retained by an embedded Vivido window.
pub struct EmbeddedFrame<'a> {
    texture: &'a wgpu::Texture,
    pub size: PhysicalSize<u32>,
}

/// Placement of an embedded frame in a window render target.
pub struct EmbeddedFramePlacement<'a> {
    pub frame: EmbeddedFrame<'a>,
    pub origin: PhysicalPosition<u32>,
}

/// Main-thread render context shared by every window surface.
///
/// Vello keeps adapters/devices in the context, so reusing it prevents each terminal pane and
/// host chrome surface from allocating another wgpu device. A renderer is cached per compatible
/// device and driven serially by the application's event loop; surfaces and render targets remain
/// window-local.
#[derive(Clone)]
pub struct SharedRenderContext(Rc<RefCell<SharedRenderState>>);

struct SharedRenderState {
    context: RenderContext,
    renderers: Vec<Option<Rc<RefCell<Renderer>>>>,
}

impl SharedRenderState {
    fn renderer_for(
        &mut self,
        device_id: usize,
        device: &wgpu::Device,
    ) -> Result<Rc<RefCell<Renderer>>, vello::Error> {
        if self.renderers.len() <= device_id {
            self.renderers.resize_with(device_id + 1, || None);
        }
        if let Some(renderer) = &self.renderers[device_id] {
            return Ok(Rc::clone(renderer));
        }

        let renderer = Rc::new(RefCell::new(create_renderer(device)?));
        self.renderers[device_id] = Some(Rc::clone(&renderer));
        Ok(renderer)
    }
}

impl SharedRenderContext {
    pub fn new() -> Self {
        Self(Rc::new(RefCell::new(SharedRenderState {
            context: RenderContext::new(),
            renderers: Vec::new(),
        })))
    }
}

impl Default for SharedRenderContext {
    fn default() -> Self {
        Self::new()
    }
}

thread_local! {
    static WINDOW_RENDER_CONTEXT: RefCell<Option<SharedRenderContext>> = const { RefCell::new(None) };
}

fn window_render_context() -> SharedRenderContext {
    WINDOW_RENDER_CONTEXT
        .with(|slot| slot.borrow_mut().get_or_insert_with(SharedRenderContext::new).clone())
}

/// Drop the main-thread window render context before platform thread-local teardown begins.
pub fn shutdown_window_render_context() {
    WINDOW_RENDER_CONTEXT.with(|slot| {
        slot.borrow_mut().take();
    });
}

impl SceneRenderer {
    pub fn new(
        source: RenderSource,
        size: PhysicalSize<u32>,
        transparent: bool,
    ) -> Result<Self, Error> {
        let size = clamp_render_size(size, wgpu::Limits::default().max_texture_dimension_2d);
        let valid_target = size.width != 0 && size.height != 0;

        let (context, surface, device, queue, target_size, renderer) = match source {
            RenderSource::Surface(window) => {
                let context = window_render_context();
                let mut context_ref = context.0.borrow_mut();
                let mut surface = block_on(context_ref.context.create_surface(
                    window,
                    size.width.max(1),
                    size.height.max(1),
                    wgpu::PresentMode::AutoVsync,
                ))
                .map_err(Error::CreateSurface)?;

                let alpha_modes = &surface
                    .surface
                    .get_capabilities(context_ref.context.devices[surface.dev_id].adapter())
                    .alpha_modes;
                surface.config.alpha_mode = surface_alpha_mode(alpha_modes);
                context_ref.context.configure_surface(&surface);

                if surface.config.alpha_mode == wgpu::CompositeAlphaMode::PostMultiplied {
                    log::info!("Surface alpha mode: {:?}", surface.config.alpha_mode);
                } else if transparent {
                    log::warn!(
                        "Window transparency is unavailable; the render surface does not support \
                         post-multiplied alpha (supported modes: {alpha_modes:?})"
                    );
                }

                let handle = &context_ref.context.devices[surface.dev_id];
                let (device, queue) = (handle.device.clone(), handle.queue.clone());
                let renderer = context_ref
                    .renderer_for(surface.dev_id, &device)
                    .map_err(Error::CreateRenderer)?;
                let target_size = PhysicalSize::new(surface.config.width, surface.config.height);
                drop(context_ref);
                (Some(context), Some(Box::new(surface)), device, queue, target_size, renderer)
            },
            RenderSource::Offscreen => {
                let (device, queue) = offscreen_device()?;
                // Nothing can refuse an offscreen size, but keep the same minimum-1 clamp so the
                // texture is always creatable.
                let target_size = PhysicalSize::new(size.width.max(1), size.height.max(1));
                let renderer =
                    Rc::new(RefCell::new(create_renderer(&device).map_err(Error::CreateRenderer)?));
                (None, None, device, queue, target_size, renderer)
            },
            RenderSource::Embedded => {
                let context = window_render_context();
                let mut context_ref = context.0.borrow_mut();
                let Some(handle) = context_ref.context.devices.first() else {
                    return Err(Error::NoWindowDevice);
                };
                let (device, queue) = (handle.device.clone(), handle.queue.clone());
                let renderer =
                    context_ref.renderer_for(0, &device).map_err(Error::CreateRenderer)?;
                let target_size = PhysicalSize::new(size.width.max(1), size.height.max(1));
                drop(context_ref);
                (Some(context), None, device, queue, target_size, renderer)
            },
        };

        let max_surface_dimension = device.limits().max_texture_dimension_2d.min(8192);
        let media = VividMediaRenderer::new(&device);
        let (render_target, render_target_view) =
            create_render_target(&device, target_size.width, target_size.height);

        Ok(Self {
            context,
            surface,
            renderer,
            device,
            queue,
            valid_target,
            max_surface_dimension,
            media,
            render_target,
            render_target_view,
            target_size,
            has_rendered_frame: false,
        })
    }

    pub fn resize(&mut self, size: PhysicalSize<u32>) {
        let size = self.clamp_render_size(size);
        if size.width == 0 || size.height == 0 {
            self.valid_target = false;
            self.has_rendered_frame = false;
            return;
        }

        if let (Some(context), Some(surface)) = (&self.context, &mut self.surface) {
            context.0.borrow().context.resize_surface(surface, size.width, size.height);
        }
        (self.render_target, self.render_target_view) =
            create_render_target(&self.device, size.width, size.height);
        self.target_size = size;
        self.valid_target = true;
        self.has_rendered_frame = false;
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
        let mut renderer = self.renderer.borrow_mut();
        self.media.draw(&self.device, &self.queue, &mut renderer, size, display_offset)
    }

    pub fn render(&mut self, scene: &Scene, base_color: Color) -> Result<bool, Error> {
        self.render_composited(scene, base_color, &[])
    }

    pub fn render_composited(
        &mut self,
        scene: &Scene,
        base_color: Color,
        frames: &[EmbeddedFramePlacement<'_>],
    ) -> Result<bool, Error> {
        if !self.valid_target {
            return Ok(false);
        }

        let PhysicalSize { width, height } = self.target_size;

        // Acquire the swapchain image first: when there is a surface and it is not presentable,
        // there is no point painting the frame at all.
        let surface_texture = match &self.surface {
            Some(surface) => match surface.surface.get_current_texture() {
                wgpu::CurrentSurfaceTexture::Success(texture)
                | wgpu::CurrentSurfaceTexture::Suboptimal(texture) => Some(texture),
                wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                    return Ok(false);
                },
                wgpu::CurrentSurfaceTexture::Lost | wgpu::CurrentSurfaceTexture::Outdated => {
                    if let (Some(context), Some(surface)) = (&self.context, &mut self.surface) {
                        context.0.borrow().context.resize_surface(
                            surface,
                            width.max(1),
                            height.max(1),
                        );
                    }
                    return Ok(false);
                },
                wgpu::CurrentSurfaceTexture::Validation => return Err(Error::SurfaceValidation),
            },
            None => None,
        };

        self.renderer
            .borrow_mut()
            .render_to_texture(
                &self.device,
                &self.queue,
                scene,
                &self.render_target_view,
                &RenderParams { base_color, width, height, antialiasing_method: AaConfig::Msaa8 },
            )
            .map_err(Error::Render)?;

        if !frames.is_empty() {
            let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("Vivido.vello.embedded_composite"),
            });
            for placement in frames {
                let PhysicalSize { width, height } =
                    embedded_copy_extent(placement.frame.size, placement.origin, self.target_size);
                if width == 0 || height == 0 {
                    continue;
                }
                encoder.copy_texture_to_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: placement.frame.texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    wgpu::TexelCopyTextureInfo {
                        texture: &self.render_target,
                        mip_level: 0,
                        origin: wgpu::Origin3d {
                            x: placement.origin.x,
                            y: placement.origin.y,
                            z: 0,
                        },
                        aspect: wgpu::TextureAspect::All,
                    },
                    wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
                );
            }
            self.queue.submit([encoder.finish()]);
        }

        // The offscreen target is the finished image; only a windowed renderer blits and presents.
        if let (Some(surface), Some(surface_texture)) = (&self.surface, surface_texture) {
            let surface_view =
                surface_texture.texture.create_view(&wgpu::TextureViewDescriptor::default());
            let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("Vivido.vello.surface_blit"),
            });
            surface.blitter.copy(
                &self.device,
                &mut encoder,
                &self.render_target_view,
                &surface_view,
            );
            self.queue.submit([encoder.finish()]);
            surface_texture.present();
        }

        self.has_rendered_frame = true;
        let _ = self.device.poll(wgpu::PollType::Poll);

        Ok(true)
    }

    pub fn embedded_frame(&self) -> Option<EmbeddedFrame<'_>> {
        self.has_rendered_frame
            .then_some(EmbeddedFrame { texture: &self.render_target, size: self.target_size })
    }

    /// Whether the render target holds a frame a screenshot could read.
    #[cfg(any(unix, windows))]
    pub fn has_rendered_frame(&self) -> bool {
        self.has_rendered_frame
    }

    /// Start asynchronously reading the last rendered frame.
    #[cfg(any(unix, windows))]
    pub fn begin_screenshot(&self) -> Result<ScreenshotReadback, ScreenshotError> {
        if !self.has_rendered_frame {
            return Err(ScreenshotError::NoPresentedFrame);
        }

        let PhysicalSize { width, height } = self.target_size;
        let (padded_bytes_per_row, buffer_size) = screenshot_layout(width, height)?;

        let device_handle = &self.device;
        let readback = device_handle.create_buffer(&wgpu::BufferDescriptor {
            label: Some("vivido.screenshot.readback"),
            size: buffer_size,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device_handle.create_command_encoder(&wgpu::CommandEncoderDescriptor {
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
        self.queue.submit([encoder.finish()]);

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
            redactions: self.media.capture_redactions().to_vec(),
        })
    }

    /// Poll an asynchronous screenshot without blocking the renderer thread.
    #[cfg(any(unix, windows))]
    pub fn poll_screenshot(
        &self,
        readback: &ScreenshotReadback,
    ) -> Result<Option<ScreenshotPixels>, ScreenshotError> {
        self.device
            .poll(wgpu::PollType::Poll)
            .map_err(|err| ScreenshotError::Device(err.to_string()))?;

        match readback.receiver.try_recv() {
            Ok(Ok(bytes)) => Ok(Some(ScreenshotPixels {
                bytes,
                width: readback.width,
                height: readback.height,
                padded_bytes_per_row: readback.padded_bytes_per_row,
                redactions: readback.redactions.clone(),
            })),
            Ok(Err(err)) => Err(ScreenshotError::Readback(err)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => {
                Err(ScreenshotError::Readback(String::from("readback channel disconnected")))
            },
        }
    }
}

fn embedded_copy_extent(
    source: PhysicalSize<u32>,
    origin: PhysicalPosition<u32>,
    target: PhysicalSize<u32>,
) -> PhysicalSize<u32> {
    PhysicalSize::new(
        source.width.min(target.width.saturating_sub(origin.x)),
        source.height.min(target.height.saturating_sub(origin.y)),
    )
}

impl Drop for SceneRenderer {
    fn drop(&mut self) {
        self.media.clear_target(&mut self.renderer.borrow_mut());
    }
}

fn create_renderer(device: &wgpu::Device) -> Result<Renderer, vello::Error> {
    Renderer::new(
        device,
        RendererOptions {
            antialiasing_support: [AaConfig::Msaa8].into_iter().collect::<AaSupport>(),
            ..RendererOptions::default()
        },
    )
}

/// Create a wgpu device with no surface behind it.
///
/// Tries a real GPU first, then falls back to a software adapter (lavapipe on Linux, WARP on
/// Windows) so a headless terminal still renders on a machine with no display hardware at all.
fn offscreen_device() -> Result<(wgpu::Device, wgpu::Queue), Error> {
    let instance = wgpu::Instance::default();

    let mut attempts = Vec::new();
    for (power, fallback) in [
        (wgpu::PowerPreference::HighPerformance, false),
        (wgpu::PowerPreference::LowPower, false),
        (wgpu::PowerPreference::LowPower, true),
    ] {
        let options = wgpu::RequestAdapterOptions {
            power_preference: power,
            force_fallback_adapter: fallback,
            compatible_surface: None,
        };
        match block_on(instance.request_adapter(&options)) {
            Ok(adapter) => {
                let info = adapter.get_info();
                log::info!(
                    "Headless adapter: {} ({:?}, {:?}) via {:?}",
                    info.name,
                    info.device_type,
                    info.backend,
                    power
                );
                if info.device_type == wgpu::DeviceType::Cpu {
                    log::warn!("Using a software renderer; frames will be slow to produce");
                }

                return block_on(adapter.request_device(&wgpu::DeviceDescriptor {
                    label: Some("vivido.headless"),
                    ..Default::default()
                }))
                .map_err(|err| Error::NoOffscreenAdapter(err.to_string()));
            },
            Err(err) => attempts
                .push(format!("{power:?}{}: {err}", if fallback { " (fallback)" } else { "" })),
        }
    }

    Err(Error::NoOffscreenAdapter(format!(
        "no wgpu adapter is available for offscreen rendering. Install a Vulkan driver, or a \
         software implementation such as Mesa's lavapipe (mesa-vulkan-drivers), to run headless \
         on a machine with no GPU. Attempts: {}",
        attempts.join("; ")
    )))
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
            | wgpu::TextureUsages::COPY_SRC
            | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    (texture, view)
}

#[cfg(any(unix, windows, test))]
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
    use super::{
        RenderSource, SceneRenderer, SharedRenderContext, clamp_render_size, embedded_copy_extent,
        offscreen_device, screenshot_layout, shutdown_window_render_context, surface_alpha_mode,
        window_render_context,
    };
    use std::rc::Rc;
    use std::sync::{Mutex, MutexGuard};

    use vello::peniko::Color;
    use vello::wgpu::CompositeAlphaMode;
    use vello::{Scene, kurbo};
    use winit::dpi::{PhysicalPosition, PhysicalSize};

    /// Serializes tests that create a wgpu device.
    ///
    /// Several drivers fault when two devices are created concurrently in one process, and the
    /// test harness runs tests on parallel threads by default.
    static GPU: Mutex<()> = Mutex::new(());

    #[test]
    fn window_renderers_reuse_the_thread_render_context() {
        let first = window_render_context();
        let second = window_render_context();
        assert!(Rc::ptr_eq(&first.0, &second.0));
    }

    #[test]
    fn shutting_down_window_render_context_allows_clean_recreation() {
        let first = window_render_context();
        shutdown_window_render_context();
        let second = window_render_context();
        assert!(!Rc::ptr_eq(&first.0, &second.0));
        shutdown_window_render_context();
    }

    fn gpu_lock() -> MutexGuard<'static, ()> {
        GPU.lock().unwrap_or_else(|err| err.into_inner())
    }

    #[test]
    fn shared_context_reuses_one_renderer_per_device() {
        let _gpu = gpu_lock();
        let Ok((device, _)) = offscreen_device() else {
            eprintln!("Skipping shared renderer test: no wgpu adapter");
            return;
        };
        let context = SharedRenderContext::new();
        let first = context.0.borrow_mut().renderer_for(0, &device).expect("first renderer");
        let second = context.0.borrow_mut().renderer_for(0, &device).expect("shared renderer");
        assert!(Rc::ptr_eq(&first, &second));
    }

    /// Render a solid frame with no windowing system and read the pixels back.
    ///
    /// This is the whole basis of headless `msg screenshot`: the offscreen target *is* the
    /// finished image, so a frame is readable without any surface ever being presented.
    #[cfg(any(unix, windows))]
    #[test]
    fn offscreen_renderer_produces_a_readable_frame() {
        let _gpu = gpu_lock();
        if offscreen_device().is_err() {
            eprintln!("Skipping offscreen renderer test: no wgpu adapter");
            return;
        }

        let size = PhysicalSize::new(64, 32);
        let mut renderer =
            SceneRenderer::new(RenderSource::Offscreen, size, false).expect("offscreen renderer");

        // Nothing has been rendered, so a screenshot must be refused rather than return garbage.
        assert!(renderer.begin_screenshot().is_err(), "screenshot before the first frame");

        let mut scene = Scene::new();
        scene.fill(
            vello::peniko::Fill::NonZero,
            kurbo::Affine::IDENTITY,
            Color::from_rgb8(0, 0, 255),
            None,
            &kurbo::Rect::new(0., 0., 64., 32.),
        );
        assert!(renderer.render(&scene, Color::from_rgb8(0, 0, 255)).expect("render"));

        let readback = renderer.begin_screenshot().expect("screenshot after a frame");
        assert_eq!((readback.width, readback.height), (64, 32));

        let pixels = loop {
            if let Some(pixels) = renderer.poll_screenshot(&readback).expect("poll") {
                break pixels;
            }
        };

        // Every row is padded to a 256-byte alignment; check the first pixel of each row.
        for row in 0..pixels.height as usize {
            let offset = row * pixels.padded_bytes_per_row as usize;
            assert_eq!(
                &pixels.bytes[offset..offset + 4],
                &[0, 0, 255, 255],
                "row {row} was not the rendered colour"
            );
        }
    }

    /// Resizing an offscreen renderer must retarget without a surface to reconfigure.
    #[cfg(any(unix, windows))]
    #[test]
    fn offscreen_renderer_resizes() {
        let _gpu = gpu_lock();
        if offscreen_device().is_err() {
            eprintln!("Skipping offscreen resize test: no wgpu adapter");
            return;
        }

        let mut renderer =
            SceneRenderer::new(RenderSource::Offscreen, PhysicalSize::new(32, 16), false)
                .expect("offscreen renderer");
        renderer.render(&Scene::new(), Color::BLACK).expect("render");
        assert!(renderer.begin_screenshot().is_ok());

        renderer.resize(PhysicalSize::new(80, 48));
        // A resize invalidates the retained frame, exactly as it does for a windowed renderer.
        assert!(renderer.begin_screenshot().is_err(), "resize must drop the retained frame");

        renderer.render(&Scene::new(), Color::BLACK).expect("render");
        let readback = renderer.begin_screenshot().expect("screenshot after resize");
        assert_eq!((readback.width, readback.height), (80, 48));
    }

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
    fn embedded_copy_extent_clips_at_target_edges() {
        assert_eq!(
            embedded_copy_extent(
                PhysicalSize::new(320, 200),
                PhysicalPosition::new(900, 550),
                PhysicalSize::new(1000, 600),
            ),
            PhysicalSize::new(100, 50),
        );
        assert_eq!(
            embedded_copy_extent(
                PhysicalSize::new(10, 10),
                PhysicalPosition::new(1001, 601),
                PhysicalSize::new(1000, 600),
            ),
            PhysicalSize::new(0, 0),
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
