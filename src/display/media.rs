//! WebGPU compositor for Vivid media and compatibility graphics commands.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use bytemuck::{Pod, Zeroable};
use vello::peniko::{ImageAlphaType, ImageData};
use vello::wgpu::util::DeviceExt;
use vello::{Renderer, wgpu};

use crate::terminal::graphics::{DeleteTarget, GraphicsCommand, GraphicsProtocol};

use crate::display::SizeInfo;
use crate::vivid::scene::{
    MAX_RENDER_ITEMS, PresentationRejection, RenderItem, SharedScene, TrackKey,
};

/// Quads the geometry buffer starts out able to hold. It grows on demand up to
/// [`MAX_RENDER_ITEMS`]; this is only the size that avoids reallocating for ordinary scenes.
const INITIAL_QUADS: usize = 256;
const VERTICES_PER_QUAD: usize = 6;

/// How long a track's texture outlives the last frame it was rendered in.
///
/// A node stops rendering for reasons that are usually momentary: a `vvmux` tab switch, a scrolled
/// out anchor, a surface hidden while its producer redraws. Dropping the texture the instant that
/// happens forces a full re-upload of every pixel when it comes back, which is the most expensive
/// thing the compositor does. A few seconds covers a switch away and back without keeping memory
/// alive for a track that has genuinely gone.
const TEXTURE_GRACE: Duration = Duration::from_secs(5);

/// Pixels the retained — that is, not currently rendered — textures may occupy in total.
///
/// This is the presenter's own ceiling on GPU memory, not the `Resource::RetainedPixels` figure the
/// resource contract grants each session: that one bounds what a producer may ask the presenter to
/// keep, per session, in ordinary memory, and honouring it here for sixteen sessions at once would
/// mean holding gigabytes of VRAM for things nobody can see. Thirty-two million pixels is 128 MiB
/// of RGBA8 — several 4K desktops, or every tab of a large `vvmux` layout — which is as much as a
/// switch-away-and-back can plausibly need.
const MAX_RETAINED_PIXELS: u64 = 32 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureRedaction {
    pub left: u32,
    pub top: u32,
    pub right: u32,
    pub bottom: u32,
}

const SHADER: &str = r#"
struct VertexInput {
    @location(0) position: vec2<f32>,
    @location(1) tex_coord: vec2<f32>,
};

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) tex_coord: vec2<f32>,
};

@group(0) @binding(0) var vivid_texture: texture_2d<f32>;
@group(0) @binding(1) var vivid_sampler: sampler;
struct SourceOptions {
    straight_alpha: u32,
};
@group(0) @binding(2) var<uniform> source_options: SourceOptions;

@vertex
fn vs_main(input: VertexInput) -> VertexOutput {
    var output: VertexOutput;
    output.position = vec4<f32>(input.position, 0.0, 1.0);
    output.tex_coord = input.tex_coord;
    return output;
}

@fragment
fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    var color = textureSample(vivid_texture, vivid_sampler, input.tex_coord);
    if source_options.straight_alpha != 0u {
        color = vec4<f32>(color.rgb * color.a, color.a);
    }
    return color;
}
"#;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Vertex {
    position: [f32; 2],
    tex_coord: [f32; 2],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct SourceOptions {
    straight_alpha: u32,
    _padding: [u32; 3],
}

struct SourceTexture {
    texture: wgpu::Texture,
    _options: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    frame_id: u64,
    pts_us: i64,
    width: u32,
    height: u32,
    rgba_ptr: usize,
    rgba_len: usize,
    alpha_mode: u64,
    last_rendered: Instant,
}

impl SourceTexture {
    fn pixels(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SourceUploadMetrics {
    pub frames: u64,
    pub uploaded_pixels: u64,
    pub full_frame_pixels: u64,
    pub media_passes: u64,
    pub skipped_passes: u64,
}

#[derive(Debug, Clone)]
pub struct PreparedMedia {
    pub image: ImageData,
    pub image_generation: u64,
    pub changed: bool,
}

struct MediaTarget {
    _texture: wgpu::Texture,
    view: wgpu::TextureView,
    image: ImageData,
    width: u32,
    height: u32,
}

/// Vivid media renderer sharing Vello's wgpu device and queue.
pub struct VividMediaRenderer {
    reported_protocols: HashSet<GraphicsProtocol>,
    bind_group_layout: wgpu::BindGroupLayout,
    pipeline: wgpu::RenderPipeline,
    sampler: wgpu::Sampler,
    vertex_buffer: wgpu::Buffer,
    vertex_capacity_quads: usize,
    reported_item_overflow: bool,
    tracks: HashMap<TrackKey, SourceTexture>,
    target: Option<MediaTarget>,
    scene: Option<SharedScene>,
    capture_redactions: Vec<CaptureRedaction>,
    source_upload_metrics: SourceUploadMetrics,
    last_snapshot: Option<(u64, u64, u64, u32, u32, usize)>,
    image_generation: u64,
    rendered: Vec<(usize, [Vertex; 6])>,
    active_tracks: HashSet<TrackKey>,
    upload_tracks: HashSet<TrackKey>,
    vertex_data: Vec<Vertex>,
}

impl VividMediaRenderer {
    pub fn new(device: &wgpu::Device) -> Self {
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("vivido.vivid.bind_group_layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("vivido.vivid.shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("vivido.vivid.pipeline_layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("vivido.vivid.pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<Vertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &[
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x2,
                            offset: 0,
                            shader_location: 0,
                        },
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x2,
                            offset: 8,
                            shader_location: 1,
                        },
                    ],
                }],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            multiview_mask: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("vivido.vivid.linear_sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let vertex_buffer = create_vertex_buffer(device, INITIAL_QUADS);
        Self {
            reported_protocols: HashSet::new(),
            bind_group_layout,
            pipeline,
            sampler,
            vertex_buffer,
            vertex_capacity_quads: INITIAL_QUADS,
            reported_item_overflow: false,
            tracks: HashMap::new(),
            target: None,
            scene: None,
            capture_redactions: Vec::new(),
            source_upload_metrics: SourceUploadMetrics::default(),
            last_snapshot: None,
            image_generation: 0,
            rendered: Vec::with_capacity(INITIAL_QUADS),
            active_tracks: HashSet::new(),
            upload_tracks: HashSet::new(),
            vertex_data: Vec::with_capacity(INITIAL_QUADS * 6),
        }
    }

    pub fn set_scene(&mut self, scene: SharedScene) {
        self.scene = Some(scene);
    }

    pub fn submit(&mut self, command: GraphicsCommand) {
        match command {
            GraphicsCommand::Transmit { protocol, .. }
                if self.reported_protocols.insert(protocol.clone()) =>
            {
                log::warn!(
                    "Compatibility graphics protocol {protocol:?} is recognized but not rendered; use Vivid"
                );
            },
            GraphicsCommand::Delete(DeleteTarget::All) => self.clear_sources(),
            _ => (),
        }
    }

    pub fn clear_sources(&mut self) {
        self.reported_protocols.clear();
        self.tracks.clear();
    }

    /// Cumulative track-texture traffic for targeted-performance diagnostics.
    pub fn source_upload_metrics(&self) -> SourceUploadMetrics {
        self.source_upload_metrics
    }

    pub fn clear_target(&mut self, renderer: &mut Renderer) {
        if let Some(target) = self.target.take() {
            renderer.unregister_texture(target.image);
        }
        self.last_snapshot = None;
    }

    pub fn draw(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        renderer: &mut Renderer,
        size: &SizeInfo,
        display_offset: usize,
    ) -> Option<PreparedMedia> {
        let Some(scene) = self.scene.as_ref().cloned() else {
            self.capture_redactions.clear();
            return None;
        };
        let snapshot = scene.snapshot();
        let items = &snapshot.items;
        let width = size.width() as u32;
        let height = size.height() as u32;
        let snapshot_key = (
            snapshot.topology_revision,
            snapshot.content_revision,
            snapshot.capture_revision,
            width,
            height,
            display_offset,
        );
        if self.last_snapshot == Some(snapshot_key) {
            self.source_upload_metrics.skipped_passes =
                self.source_upload_metrics.skipped_passes.saturating_add(1);
            return self.target.as_ref().map(|target| PreparedMedia {
                image: target.image.clone(),
                image_generation: self.image_generation,
                changed: false,
            });
        }
        // The target owns what a placement unit means: cells for a terminal, logical pixels for a
        // desktop. The renderer only needs the conversion.
        let placement_scale = scene.target().placement_scale(size);
        if items.is_empty() && self.target.is_none() {
            // An empty scene is usually a momentary one — everything hidden while a producer
            // redraws — so the textures are retained on the same terms as any other frame in which
            // a track does not render.
            self.retain_tracks(&HashSet::new(), Instant::now());
            self.capture_redactions.clear();
            self.last_snapshot = Some(snapshot_key);
            self.source_upload_metrics.skipped_passes =
                self.source_upload_metrics.skipped_passes.saturating_add(1);
            return None;
        }
        self.ensure_target(device, renderer, width, height);

        self.rendered.clear();
        self.rendered.extend(items.iter().enumerate().filter_map(|(index, item)| {
            vertices(item, size, display_offset, placement_scale).map(|vertices| (index, vertices))
        }));
        // `MAX_RENDER_ITEMS` is the scene's own ceiling, so this should be unreachable. Clamp
        // anyway: writing geometry past the end of the buffer is a wgpu validation error, and the
        // default handler turns that into a panic that takes the window with it.
        if self.rendered.len() > MAX_RENDER_ITEMS {
            if !self.reported_item_overflow {
                self.reported_item_overflow = true;
                log::warn!(
                    "Vivid scene produced {} render items; drawing the first {MAX_RENDER_ITEMS}",
                    self.rendered.len()
                );
            }
            self.rendered.truncate(MAX_RENDER_ITEMS);
        }
        self.ensure_vertex_capacity(device, self.rendered.len());
        self.capture_redactions.clear();
        self.capture_redactions.extend(
            self.rendered
                .iter()
                .filter(|(index, _)| {
                    let item = &items[*index];
                    item.capture_policy & vivid_protocol::surface::POLICY_DENY_CAPTURE != 0
                })
                .map(|(_, vertices)| redaction_from_vertices(vertices, size)),
        );
        self.active_tracks.clear();
        self.active_tracks.extend(self.rendered.iter().map(|(index, _)| items[*index].track_key));
        let active = std::mem::take(&mut self.active_tracks);
        self.retain_tracks(&active, Instant::now());
        self.active_tracks = active;
        self.upload_tracks.clear();
        self.upload_tracks.extend(self.rendered.iter().map(|(index, _)| items[*index].track_key));
        let upload_tracks = std::mem::take(&mut self.upload_tracks);
        for key in &upload_tracks {
            let Some(item) = items.iter().find(|item| item.track_key == *key) else {
                continue;
            };
            debug_assert_eq!(item.track_key.surface, item.surface_key);
            self.upload_track(device, queue, item.track_key, item);
        }
        self.upload_tracks = upload_tracks;

        self.vertex_data.clear();
        self.vertex_data.extend(self.rendered.iter().flat_map(|(_, vertices)| *vertices));
        if !self.vertex_data.is_empty() {
            queue.write_buffer(&self.vertex_buffer, 0, bytemuck::cast_slice(&self.vertex_data));
        }

        let target = self.target.as_ref().expect("media target initialized");
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("vivido.vivid.encoder"),
        });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("vivido.vivid.pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target.view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_vertex_buffer(0, self.vertex_buffer.slice(..));
            for (index, (item_index, _)) in self.rendered.iter().enumerate() {
                let item = &items[*item_index];
                if let Some(track) = self.tracks.get(&item.track_key) {
                    pass.set_bind_group(0, &track.bind_group, &[]);
                    let start = (index * 6) as u32;
                    pass.draw(start..start + 6, 0..1);
                }
            }
        }
        queue.submit([encoder.finish()]);
        self.source_upload_metrics.media_passes =
            self.source_upload_metrics.media_passes.saturating_add(1);
        for (item_index, _) in &self.rendered {
            let item = &items[*item_index];
            match scene.mark_presented(
                item.track_key,
                item.channel_generation,
                item.surface_generation,
                item.frame.frame_id,
                item.frame.pts_us,
            ) {
                Ok(()) => {},
                // Every resize supersedes frames already in flight. Reporting that as a presenter
                // problem puts a warning on screen for ordinary window movement.
                Err(error @ PresentationRejection::Superseded(_)) => log::debug!(
                    "Vivid presentation for track {:?} was superseded: {error}",
                    item.track_key
                ),
                Err(error @ PresentationRejection::Failed(_)) => log::warn!(
                    "Could not record Vivid presentation for track {:?}: {error}",
                    item.track_key
                ),
            }
        }
        renderer.mark_override_image_dirty(&target.image);
        self.last_snapshot = Some(snapshot_key);
        Some(PreparedMedia {
            image: target.image.clone(),
            image_generation: self.image_generation,
            changed: true,
        })
    }

    #[cfg(any(unix, windows))]
    pub fn capture_redactions(&self) -> &[CaptureRedaction] {
        &self.capture_redactions
    }

    /// Keep the textures of tracks that are not rendering this frame, within bounds.
    ///
    /// A track that stops rendering used to lose its texture on that very frame, so coming back
    /// re-uploaded every pixel. Instead its texture survives [`TEXTURE_GRACE`], and the retained
    /// set as a whole survives only while it fits [`MAX_RETAINED_PIXELS`] — past that the tracks
    /// that have gone unrendered longest are dropped first, since they are the least likely to be
    /// wanted next.
    fn retain_tracks(&mut self, active: &HashSet<TrackKey>, now: Instant) {
        let mut retained = Vec::new();
        for (key, track) in &mut self.tracks {
            if active.contains(key) {
                track.last_rendered = now;
            } else {
                retained.push((
                    *key,
                    now.saturating_duration_since(track.last_rendered),
                    track.pixels(),
                ));
            }
        }
        for key in evictions(&mut retained) {
            self.tracks.remove(&key);
        }
    }

    fn upload_track(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        key: TrackKey,
        item: &RenderItem,
    ) {
        let frame = &item.frame;
        let unchanged = self.tracks.get(&key).is_some_and(|track| {
            track.frame_id == frame.frame_id
                && track.pts_us == frame.pts_us
                && track.width == frame.width
                && track.height == frame.height
                && track.rgba_ptr == frame.rgba.as_ptr() as usize
                && track.rgba_len == frame.rgba.len()
        });
        if unchanged {
            return;
        }
        let full_frame_pixels = u64::from(frame.width) * u64::from(frame.height);
        if let Some(track) = self.tracks.get_mut(&key)
            && track.width == frame.width
            && track.height == frame.height
            && track.alpha_mode == frame.alpha_mode
        {
            let uploaded_pixels = if let Some(damage) = &frame.damage {
                for rect in damage.iter() {
                    let offset = (rect.y as usize * frame.width as usize + rect.x as usize) * 4;
                    queue.write_texture(
                        wgpu::TexelCopyTextureInfo {
                            texture: &track.texture,
                            mip_level: 0,
                            origin: wgpu::Origin3d { x: rect.x, y: rect.y, z: 0 },
                            aspect: wgpu::TextureAspect::All,
                        },
                        &frame.rgba[offset..],
                        wgpu::TexelCopyBufferLayout {
                            offset: 0,
                            bytes_per_row: Some(frame.width * 4),
                            rows_per_image: None,
                        },
                        wgpu::Extent3d {
                            width: rect.width,
                            height: rect.height,
                            depth_or_array_layers: 1,
                        },
                    );
                }
                damage.iter().fold(0_u64, |total, rect| {
                    total.saturating_add(u64::from(rect.width) * u64::from(rect.height))
                })
            } else {
                queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: &track.texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    frame.rgba.as_ref(),
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(frame.width * 4),
                        rows_per_image: Some(frame.height),
                    },
                    wgpu::Extent3d {
                        width: frame.width,
                        height: frame.height,
                        depth_or_array_layers: 1,
                    },
                );
                full_frame_pixels
            };
            track.frame_id = frame.frame_id;
            track.pts_us = frame.pts_us;
            track.rgba_ptr = frame.rgba.as_ptr() as usize;
            track.rgba_len = frame.rgba.len();
            self.source_upload_metrics.frames = self.source_upload_metrics.frames.saturating_add(1);
            self.source_upload_metrics.uploaded_pixels =
                self.source_upload_metrics.uploaded_pixels.saturating_add(uploaded_pixels);
            self.source_upload_metrics.full_frame_pixels =
                self.source_upload_metrics.full_frame_pixels.saturating_add(full_frame_pixels);
            return;
        }
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("vivido.vivid.track"),
            size: wgpu::Extent3d {
                width: frame.width,
                height: frame.height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            frame.rgba.as_ref(),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(frame.width * 4),
                rows_per_image: Some(frame.height),
            },
            wgpu::Extent3d { width: frame.width, height: frame.height, depth_or_array_layers: 1 },
        );
        let options = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("vivido.vivid.source_options"),
            contents: bytemuck::bytes_of(&SourceOptions {
                straight_alpha: u32::from(frame.alpha_mode == crate::vivid::scene::ALPHA_STRAIGHT),
                _padding: [0; 3],
            }),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let view = texture.create_view(&Default::default());
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("vivido.vivid.source_bind_group"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry { binding: 2, resource: options.as_entire_binding() },
            ],
        });
        self.tracks.insert(
            key,
            SourceTexture {
                texture,
                _options: options,
                bind_group,
                frame_id: frame.frame_id,
                pts_us: frame.pts_us,
                width: frame.width,
                height: frame.height,
                rgba_ptr: frame.rgba.as_ptr() as usize,
                rgba_len: frame.rgba.len(),
                alpha_mode: frame.alpha_mode,
                last_rendered: Instant::now(),
            },
        );
        self.source_upload_metrics.frames = self.source_upload_metrics.frames.saturating_add(1);
        self.source_upload_metrics.uploaded_pixels =
            self.source_upload_metrics.uploaded_pixels.saturating_add(full_frame_pixels);
        self.source_upload_metrics.full_frame_pixels =
            self.source_upload_metrics.full_frame_pixels.saturating_add(full_frame_pixels);
    }

    /// Make sure the geometry buffer can hold `quads`, growing it if not.
    ///
    /// The buffer only ever grows, and never past [`MAX_RENDER_ITEMS`], so a scene that briefly
    /// gets large does not reallocate on every subsequent frame.
    fn ensure_vertex_capacity(&mut self, device: &wgpu::Device, quads: usize) {
        let Some(capacity) = grown_vertex_capacity(quads, self.vertex_capacity_quads) else {
            return;
        };
        self.vertex_buffer = create_vertex_buffer(device, capacity);
        self.vertex_capacity_quads = capacity;
    }

    fn ensure_target(
        &mut self,
        device: &wgpu::Device,
        renderer: &mut Renderer,
        width: u32,
        height: u32,
    ) {
        let width = width.clamp(1, 8192);
        let height = height.clamp(1, 8192);
        if self
            .target
            .as_ref()
            .is_some_and(|target| target.width == width && target.height == height)
        {
            return;
        }
        self.clear_target(renderer);
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("vivido.vivid.target"),
            size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&Default::default());
        let mut image = renderer.register_texture(texture.clone());
        image.alpha_type = ImageAlphaType::AlphaPremultiplied;
        self.target = Some(MediaTarget { _texture: texture, view, image, width, height });
        self.image_generation = self.image_generation.wrapping_add(1);
    }
}

/// Which retained textures to drop, given how long each has gone unrendered and how big it is.
///
/// Everything past the grace period goes first, then — only if what is left still exceeds the
/// budget — the longest unrendered of the survivors, until it fits. Taking the oldest first keeps
/// the tab a user just switched away from, which is the one they are most likely to switch back to.
fn evictions<K: Copy>(retained: &mut Vec<(K, Duration, u64)>) -> Vec<K> {
    let mut evicted = Vec::new();
    retained.retain(|(key, age, _)| {
        let expired = *age >= TEXTURE_GRACE;
        if expired {
            evicted.push(*key);
        }
        !expired
    });
    let mut total =
        retained.iter().fold(0_u64, |total, (_, _, pixels)| total.saturating_add(*pixels));
    if total <= MAX_RETAINED_PIXELS {
        return evicted;
    }
    retained.sort_by_key(|(_, age, _)| std::cmp::Reverse(*age));
    for (key, _, pixels) in retained.iter() {
        if total <= MAX_RETAINED_PIXELS {
            break;
        }
        evicted.push(*key);
        total = total.saturating_sub(*pixels);
    }
    evicted
}

/// The capacity the geometry buffer must grow to in order to hold `quads`, or `None` when
/// `current` is already enough.
///
/// Doubling keeps a scene that grows steadily from reallocating every frame, and the clamp is what
/// keeps the result a capacity the caller can actually write into: `quads` is never allowed past
/// [`MAX_RENDER_ITEMS`], so clamping there can never return less than was asked for.
fn grown_vertex_capacity(quads: usize, current: usize) -> Option<usize> {
    if quads <= current {
        return None;
    }
    Some(quads.next_power_of_two().min(MAX_RENDER_ITEMS).max(quads))
}

fn create_vertex_buffer(device: &wgpu::Device, quads: usize) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("vivido.vivid.vertices"),
        size: (quads * VERTICES_PER_QUAD * std::mem::size_of::<Vertex>()) as u64,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

fn vertices(
    item: &RenderItem,
    size: &SizeInfo,
    display_offset: usize,
    scale: (f32, f32),
) -> Option<[Vertex; 6]> {
    let (scale_x, scale_y) = scale;
    let x = size.padding_x() + fixed_to_f32(item.x) * scale_x;
    let scroll = if item.text_anchored { display_offset as f32 } else { 0.0 };
    let y = size.padding_y() + (fixed_to_f32(item.y) + scroll) * scale_y;
    let output_width = fixed_to_f32(item.width) * scale_x;
    let output_height = fixed_to_f32(item.height) * scale_y;
    let display_width =
        item.frame.width as f32 * item.frame.sar_num as f32 / item.frame.sar_den.max(1) as f32;
    let scale =
        (output_width / display_width).min(output_height / item.frame.height as f32).max(0.0);
    let width = display_width * scale;
    let height = item.frame.height as f32 * scale;
    let quad_left = x + (output_width - width) * 0.5;
    let quad_top = y + (output_height - height) * 0.5;
    let quad_right = quad_left + width;
    let quad_bottom = quad_top + height;
    if width <= 0.0 || height <= 0.0 {
        return None;
    }

    let (clip_left, clip_top, clip_right, clip_bottom) =
        item.clip.map_or((0.0, 0.0, size.width(), size.height()), |clip| {
            let left = size.padding_x() + fixed_to_f32(clip.x) * scale_x;
            let top = size.padding_y() + (fixed_to_f32(clip.y) + scroll) * scale_y;
            (
                left.max(0.0),
                top.max(0.0),
                (left + fixed_to_f32(clip.width) * scale_x).min(size.width()),
                (top + fixed_to_f32(clip.height) * scale_y).min(size.height()),
            )
        });
    let pixel_left = quad_left.max(clip_left).max(0.0);
    let pixel_top = quad_top.max(clip_top).max(0.0);
    let pixel_right = quad_right.min(clip_right).min(size.width());
    let pixel_bottom = quad_bottom.min(clip_bottom).min(size.height());
    if pixel_left >= pixel_right || pixel_top >= pixel_bottom {
        return None;
    }

    let u0 = (pixel_left - quad_left) / width;
    let v0 = (pixel_top - quad_top) / height;
    let u1 = (pixel_right - quad_left) / width;
    let v1 = (pixel_bottom - quad_top) / height;
    let left = pixel_left / size.width() * 2.0 - 1.0;
    let right = pixel_right / size.width() * 2.0 - 1.0;
    let top = 1.0 - pixel_top / size.height() * 2.0;
    let bottom = 1.0 - pixel_bottom / size.height() * 2.0;
    Some([
        Vertex { position: [left, top], tex_coord: [u0, v0] },
        Vertex { position: [left, bottom], tex_coord: [u0, v1] },
        Vertex { position: [right, top], tex_coord: [u1, v0] },
        Vertex { position: [right, top], tex_coord: [u1, v0] },
        Vertex { position: [left, bottom], tex_coord: [u0, v1] },
        Vertex { position: [right, bottom], tex_coord: [u1, v1] },
    ])
}

fn redaction_from_vertices(vertices: &[Vertex; 6], size: &SizeInfo) -> CaptureRedaction {
    let width = size.width().max(0.0);
    let height = size.height().max(0.0);
    let left = ((vertices[0].position[0] + 1.0) * 0.5 * width).floor().max(0.0);
    let top = ((1.0 - vertices[0].position[1]) * 0.5 * height).floor().max(0.0);
    let right = ((vertices[5].position[0] + 1.0) * 0.5 * width).ceil().min(width);
    let bottom = ((1.0 - vertices[5].position[1]) * 0.5 * height).ceil().min(height);
    CaptureRedaction {
        left: left as u32,
        top: top as u32,
        right: right as u32,
        bottom: bottom as u32,
    }
}

fn fixed_to_f32(value: i64) -> f32 {
    (value as f64 / 4_294_967_296.0) as f32
}

#[cfg(test)]
mod protocol_renderer_tests {
    use std::time::Duration;

    use super::{
        INITIAL_QUADS, MAX_RENDER_ITEMS, MAX_RETAINED_PIXELS, TEXTURE_GRACE, evictions,
        fixed_to_f32, grown_vertex_capacity,
    };

    #[test]
    fn fixed_point_cell_coordinates_remain_fractional() {
        assert_eq!(fixed_to_f32(2_i64 << 32), 2.0);
        assert_eq!(fixed_to_f32(1_i64 << 31), 0.5);
    }

    /// The geometry buffer used to be fixed at 256 quads while the presenter admits 256 nodes per
    /// session across sixteen sessions, plus retained posters. Writing past the end of a wgpu
    /// buffer is a validation error, and the default handler turns that into a panic — so a
    /// nested `vvmux` layout with enough visible nodes took the window down.
    #[test]
    fn the_geometry_buffer_grows_to_every_scene_the_presenter_can_produce() {
        assert_eq!(grown_vertex_capacity(INITIAL_QUADS, INITIAL_QUADS), None, "no needless growth");
        assert_eq!(grown_vertex_capacity(INITIAL_QUADS + 1, INITIAL_QUADS), Some(512));

        // The ceiling has to be reachable, not merely near: the largest scene the scene module can
        // hand over must still get a capacity that holds all of it.
        let capacity = grown_vertex_capacity(MAX_RENDER_ITEMS, INITIAL_QUADS)
            .expect("the largest possible scene needs more than the initial capacity");
        assert!(
            capacity >= MAX_RENDER_ITEMS,
            "capacity {capacity} must hold {MAX_RENDER_ITEMS} quads"
        );
        assert_eq!(grown_vertex_capacity(MAX_RENDER_ITEMS, capacity), None);
    }

    /// A track that stops rendering used to lose its texture on that very frame, so a `vvmux` tab
    /// switch or a scrolled-out anchor paid a full re-upload to come back. It now survives a brief
    /// absence, and only a brief one.
    #[test]
    fn a_texture_survives_a_brief_absence_and_no_longer() {
        let mut retained = vec![
            ('a', Duration::from_millis(16), 1920 * 1080),
            ('b', TEXTURE_GRACE - Duration::from_millis(1), 1920 * 1080),
            ('c', TEXTURE_GRACE, 1920 * 1080),
            ('d', TEXTURE_GRACE + Duration::from_secs(60), 1920 * 1080),
        ];
        assert_eq!(evictions(&mut retained), vec!['c', 'd'], "only what is past the grace period");
        assert_eq!(retained.len(), 2, "the recent two are kept");
    }

    /// Retention is bounded by pixels, not by count, and gives up the longest-absent first.
    #[test]
    fn the_retained_set_stays_within_its_pixel_budget_oldest_first() {
        let each = MAX_RETAINED_PIXELS / 4;
        let mut retained = vec![
            ('a', Duration::from_millis(10), each),
            ('b', Duration::from_millis(40), each),
            ('c', Duration::from_millis(20), each),
            ('d', Duration::from_millis(30), each),
        ];
        assert!(evictions(&mut retained).is_empty(), "exactly the budget is not over it");

        let mut retained = vec![
            ('a', Duration::from_millis(10), each),
            ('b', Duration::from_millis(40), each),
            ('c', Duration::from_millis(20), each),
            ('d', Duration::from_millis(30), each),
            ('e', Duration::from_millis(50), each),
            ('f', Duration::from_millis(60), each),
        ];
        assert_eq!(
            evictions(&mut retained),
            vec!['f', 'e'],
            "the longest unrendered go, and only until the rest fits"
        );

        // One texture larger than the whole budget is not kept at any age.
        let mut huge = vec![('a', Duration::ZERO, MAX_RETAINED_PIXELS + 1)];
        assert_eq!(evictions(&mut huge), vec!['a']);
    }
}

// The protocol-facing renderer regressions live in `vivid::scene`; the legacy source-era GPU
// fixtures below are retained temporarily as migration references but are not compiled.
#[cfg(all(test, any()))]
mod tests {
    #[cfg(unix)]
    use std::sync::{Arc, mpsc};

    #[cfg(unix)]
    use vello::kurbo::Affine;
    #[cfg(unix)]
    use vello::peniko::{Color, ImageData};
    #[cfg(unix)]
    use vello::{AaConfig, RenderParams, Renderer, RendererOptions, Scene, wgpu};
    #[cfg(unix)]
    use vivid_protocol::messages::{ClipRect, RasterSourceConfig};

    #[cfg(unix)]
    use crate::display::SizeInfo;
    #[cfg(unix)]
    use crate::vivid::scene::{
        Frame, RasterDamageRect, RenderItem, SceneMutation, SceneNode, SharedScene, SourceConfig,
    };

    #[cfg(unix)]
    use super::VividMediaRenderer;
    use super::fixed_to_f32;

    #[test]
    fn fixed_point_cell_coordinates_are_fractional() {
        assert_eq!(fixed_to_f32(2_i64 << 32), 2.0);
        assert_eq!(fixed_to_f32(1_i64 << 31), 0.5);
    }

    #[cfg(unix)]
    #[test]
    fn node_clip_crops_geometry_and_uv_without_refitting() {
        let item = RenderItem {
            source_key: (1, 1),
            node_id: 1,
            frame: Frame {
                frame_id: 1,
                pts_us: 0,
                width: 4,
                height: 2,
                rgba: Arc::from(vec![255; 4 * 2 * 4]),
                alpha_mode: vivid_protocol::messages::ALPHA_STRAIGHT,
                sar_num: 1,
                sar_den: 1,
                damage: None,
            },
            x: 0,
            y: 0,
            width: 4_i64 << 32,
            height: 2_i64 << 32,
            text_layer: 1,
            z_index: 0,
            text_anchored: false,
            clip: Some(ClipRect { x: 1_i64 << 32, y: 0, width: 2_i64 << 32, height: 2_i64 << 32 }),
            capture_policy: 0,
        };
        let size = SizeInfo::new(4.0, 2.0, 1.0, 1.0, 0.0, 0.0, false);
        let clipped =
            super::vertices(&item, &size, 0, (size.cell_width(), size.cell_height())).unwrap();
        assert_eq!(clipped[0].position, [-0.5, 1.0]);
        assert_eq!(clipped[5].position, [0.5, -1.0]);
        assert_eq!(clipped[0].tex_coord, [0.25, 0.0]);
        assert_eq!(clipped[5].tex_coord, [0.75, 1.0]);

        let hidden = RenderItem {
            clip: Some(ClipRect {
                x: -(2_i64 << 32),
                y: 0,
                width: 1_i64 << 32,
                height: 1_i64 << 32,
            }),
            ..item
        };
        assert!(
            super::vertices(&hidden, &size, 0, (size.cell_width(), size.cell_height())).is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn headless_compositor_handles_straight_alpha_overlap_letterboxing_and_scroll() {
        let instance = wgpu::Instance::default();
        let Ok(adapter) =
            pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
        else {
            eprintln!("Skipping headless compositor test: no native wgpu adapter");
            return;
        };
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
                .expect("headless wgpu device");
        let mut renderer = Renderer::new(&device, RendererOptions::default()).expect("Vello");
        let mut media = VividMediaRenderer::new(&device);
        let scene = SharedScene::for_test();
        scene.add_anchor(1, 1, 0, -1).unwrap();

        for (source_id, rgba) in
            [(1, [255, 0, 0, 128]), (2, [0, 0, 255, 128]), (3, [255, 255, 255, 0])]
        {
            scene
                .add_source(
                    1,
                    source_id,
                    SourceConfig::Raster(RasterSourceConfig {
                        source_id,
                        width: 1,
                        height: 1,
                        alpha_mode: vivid_protocol::messages::ALPHA_STRAIGHT,
                        compression_mode: vivid_protocol::messages::COMPRESSION_NONE,
                    }),
                )
                .unwrap();
            scene
                .publish_frame(
                    (1, source_id),
                    0,
                    Frame {
                        frame_id: source_id,
                        pts_us: 0,
                        width: 1,
                        height: 1,
                        rgba: Arc::from(rgba),
                        alpha_mode: vivid_protocol::messages::ALPHA_STRAIGHT,
                        sar_num: 1,
                        sar_den: 1,
                        damage: None,
                    },
                )
                .unwrap();
        }
        scene
            .commit_mutations(
                1,
                (1..=3)
                    .map(|node_id| {
                        SceneMutation::Create(SceneNode {
                            session_id: 1,
                            node_id,
                            source_id: node_id,
                            x: 0,
                            y: 0,
                            width: 4_i64 << 32,
                            height: 2_i64 << 32,
                            text_layer: 1,
                            z_index: node_id as i64,
                            visible: true,
                            anchor_id: Some(1),
                            clip: None,
                        })
                    })
                    .collect(),
            )
            .unwrap();
        media.set_scene(scene.clone());

        let size = SizeInfo::new(4.0, 2.0, 1.0, 1.0, 0.0, 0.0, false);
        media.draw(&device, &queue, &mut renderer, &size, 1).expect("media image");

        let bytes_per_row = 256;
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("vivido.vivid.test_readback"),
            size: bytes_per_row * 2,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let target = &media.target.as_ref().unwrap()._texture;
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        encoder.copy_texture_to_buffer(
            target.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(bytes_per_row as u32),
                    rows_per_image: Some(2),
                },
            },
            wgpu::Extent3d { width: 4, height: 2, depth_or_array_layers: 1 },
        );
        queue.submit([encoder.finish()]);

        let (sender, receiver) = mpsc::channel();
        readback.slice(..).map_async(wgpu::MapMode::Read, move |result| {
            sender.send(result).unwrap();
        });
        device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        receiver.recv().unwrap().unwrap();
        let data = readback.slice(..).get_mapped_range();

        assert_eq!(&data[0..4], &[0, 0, 0, 0], "FIT_CONTAIN must letterbox");
        for (actual, expected) in data[4..8].iter().zip([64_u8, 0, 128, 192]) {
            assert!(actual.abs_diff(expected) <= 2, "actual={actual}, expected={expected}");
        }
        drop(data);
        readback.unmap();

        scene.remove_source((1, 3)).unwrap();
        media.draw(&device, &queue, &mut renderer, &size, 1).expect("media image");
        assert_eq!(media.sources.len(), 2, "unused GPU textures must be pruned");
    }

    #[cfg(unix)]
    #[test]
    fn registered_media_texture_refreshes_after_compositor_updates() {
        let instance = wgpu::Instance::default();
        let Ok(adapter) =
            pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
        else {
            eprintln!("Skipping registered media refresh test: no native wgpu adapter");
            return;
        };
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
                .expect("headless wgpu device");
        let mut renderer = Renderer::new(&device, RendererOptions::default()).expect("Vello");
        let mut media = VividMediaRenderer::new(&device);
        let scene = SharedScene::for_test();
        scene.add_anchor(1, 1, 0, 0).unwrap();
        scene
            .add_source(
                1,
                1,
                SourceConfig::Raster(RasterSourceConfig {
                    source_id: 1,
                    width: 1,
                    height: 1,
                    alpha_mode: vivid_protocol::messages::ALPHA_STRAIGHT,
                    compression_mode: vivid_protocol::messages::COMPRESSION_NONE,
                }),
            )
            .unwrap();
        scene
            .commit_mutations(
                1,
                vec![SceneMutation::Create(SceneNode {
                    session_id: 1,
                    node_id: 1,
                    source_id: 1,
                    x: 0,
                    y: 0,
                    width: 1_i64 << 32,
                    height: 1_i64 << 32,
                    text_layer: 1,
                    z_index: 0,
                    visible: true,
                    anchor_id: Some(1),
                    clip: None,
                })],
            )
            .unwrap();
        media.set_scene(scene.clone());
        let size = SizeInfo::new(1.0, 1.0, 1.0, 1.0, 0.0, 0.0, false);

        publish_test_frame(&scene, 1, [255, 0, 0, 255]);
        let image = media.draw(&device, &queue, &mut renderer, &size, 0).unwrap();
        assert_eq!(
            render_registered_image(&device, &queue, &mut renderer, &image),
            [255, 0, 0, 255]
        );

        publish_test_frame(&scene, 2, [0, 0, 255, 255]);
        let image = media.draw(&device, &queue, &mut renderer, &size, 0).unwrap();
        assert_eq!(
            render_registered_image(&device, &queue, &mut renderer, &image),
            [0, 0, 255, 255]
        );

        let image = media.draw(&device, &queue, &mut renderer, &size, 1).unwrap();
        assert_eq!(render_registered_image(&device, &queue, &mut renderer, &image), [0, 0, 0, 0]);
    }

    #[cfg(unix)]
    #[test]
    fn sparse_raster_damage_reuses_texture_and_reduces_upload_area() {
        let instance = wgpu::Instance::default();
        let Ok(adapter) =
            pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
        else {
            eprintln!("Skipping sparse upload test: no native wgpu adapter");
            return;
        };
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
                .expect("headless wgpu device");
        let mut media = VividMediaRenderer::new(&device);
        let base = RenderItem {
            source_key: (1, 9),
            node_id: 1,
            frame: Frame {
                frame_id: 1,
                pts_us: 0,
                width: 4,
                height: 4,
                rgba: Arc::from(vec![0; 4 * 4 * 4]),
                alpha_mode: vivid_protocol::messages::ALPHA_STRAIGHT,
                sar_num: 1,
                sar_den: 1,
                damage: None,
            },
            x: 0,
            y: 0,
            width: 4_i64 << 32,
            height: 4_i64 << 32,
            text_layer: 1,
            z_index: 0,
            text_anchored: false,
            clip: None,
            capture_policy: 0,
        };
        media.upload_source(&device, &queue, base.source_key, &base);
        let mut changed = vec![0; 4 * 4 * 4];
        changed[(2 * 4 + 1) * 4..(2 * 4 + 2) * 4].copy_from_slice(&[1, 2, 3, 255]);
        let delta = RenderItem {
            frame: Frame {
                frame_id: 2,
                pts_us: 1,
                rgba: Arc::from(changed),
                damage: Some(Arc::from([RasterDamageRect { x: 1, y: 2, width: 1, height: 1 }])),
                ..base.frame.clone()
            },
            ..base
        };
        media.upload_source(&device, &queue, delta.source_key, &delta);
        device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        assert_eq!(
            media.source_upload_metrics(),
            super::SourceUploadMetrics {
                frames: 2,
                uploaded_pixels: 17,
                full_frame_pixels: 32,
                media_passes: 0,
                skipped_passes: 0,
            }
        );
    }

    #[cfg(unix)]
    fn publish_test_frame(scene: &SharedScene, frame_id: u64, rgba: [u8; 4]) {
        scene
            .publish_frame(
                (1, 1),
                0,
                Frame {
                    frame_id,
                    pts_us: frame_id as i64,
                    width: 1,
                    height: 1,
                    rgba: Arc::from(rgba),
                    alpha_mode: vivid_protocol::messages::ALPHA_STRAIGHT,
                    sar_num: 1,
                    sar_den: 1,
                    damage: None,
                },
            )
            .unwrap();
    }

    #[cfg(unix)]
    fn render_registered_image(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        renderer: &mut Renderer,
        image: &ImageData,
    ) -> [u8; 4] {
        let mut scene = Scene::new();
        scene.draw_image(image, Affine::IDENTITY);
        let output = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("vivido.vivid.test_vello_output"),
            size: wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        renderer
            .render_to_texture(
                device,
                queue,
                &scene,
                &output.create_view(&Default::default()),
                &RenderParams {
                    base_color: Color::TRANSPARENT,
                    width: 1,
                    height: 1,
                    antialiasing_method: AaConfig::Area,
                },
            )
            .unwrap();

        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("vivido.vivid.test_vello_readback"),
            size: 256,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        encoder.copy_texture_to_buffer(
            output.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(256),
                    rows_per_image: Some(1),
                },
            },
            wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
        );
        queue.submit([encoder.finish()]);

        let (sender, receiver) = mpsc::channel();
        readback.slice(..).map_async(wgpu::MapMode::Read, move |result| {
            sender.send(result).unwrap();
        });
        device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        receiver.recv().unwrap().unwrap();
        let data = readback.slice(..).get_mapped_range();
        let rgba = data[0..4].try_into().unwrap();
        drop(data);
        readback.unmap();
        rgba
    }
}
