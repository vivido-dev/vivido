//! WebGPU compositor for Vivid media and compatibility graphics commands.

use std::collections::{HashMap, HashSet};

use bytemuck::{Pod, Zeroable};
use vello::peniko::{ImageAlphaType, ImageData};
use vello::wgpu::util::DeviceExt;
use vello::{Renderer, wgpu};

use vivido_terminal::graphics::{DeleteTarget, GraphicsCommand, GraphicsProtocol};

use crate::display::SizeInfo;
use crate::vivid::scene::{RenderItem, SharedScene, SourceKey};

const MAX_NODES: usize = 256;

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
    _texture: wgpu::Texture,
    _options: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    frame_id: u64,
    pts_us: i64,
    width: u32,
    height: u32,
    rgba_ptr: usize,
    rgba_len: usize,
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
    sources: HashMap<(u64, u64), SourceTexture>,
    target: Option<MediaTarget>,
    scene: Option<SharedScene>,
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
        let vertex_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("vivido.vivid.vertices"),
            size: (MAX_NODES * 6 * std::mem::size_of::<Vertex>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Self {
            reported_protocols: HashSet::new(),
            bind_group_layout,
            pipeline,
            sampler,
            vertex_buffer,
            sources: HashMap::new(),
            target: None,
            scene: None,
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
        self.sources.clear();
    }

    pub fn clear_target(&mut self, renderer: &mut Renderer) {
        if let Some(target) = self.target.take() {
            renderer.unregister_texture(target.image);
        }
    }

    pub fn draw(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        renderer: &mut Renderer,
        size: &SizeInfo,
        display_offset: usize,
    ) -> Option<ImageData> {
        let scene = self.scene.as_ref()?.clone();
        let (_, items) = scene.snapshot();
        if items.is_empty() {
            self.sources.clear();
            return None;
        }
        self.ensure_target(device, renderer, size.width() as u32, size.height() as u32);

        let active = items.iter().map(|item| item.source_key).collect::<HashSet<_>>();
        self.sources.retain(|key, _| active.contains(key));
        for item in &items {
            self.upload_source(device, queue, item.source_key, item);
        }

        let vertices =
            items.iter().flat_map(|item| vertices(item, size, display_offset)).collect::<Vec<_>>();
        queue.write_buffer(&self.vertex_buffer, 0, bytemuck::cast_slice(&vertices));

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
            for (index, item) in items.iter().enumerate() {
                if let Some(source) = self.sources.get(&item.source_key) {
                    pass.set_bind_group(0, &source.bind_group, &[]);
                    let start = (index * 6) as u32;
                    pass.draw(start..start + 6, 0..1);
                }
            }
        }
        queue.submit([encoder.finish()]);
        renderer.mark_override_image_dirty(&target.image);
        Some(target.image.clone())
    }

    fn upload_source(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        key: SourceKey,
        item: &RenderItem,
    ) {
        let frame = &item.frame;
        let unchanged = self.sources.get(&key).is_some_and(|source| {
            source.frame_id == frame.frame_id
                && source.pts_us == frame.pts_us
                && source.width == frame.width
                && source.height == frame.height
                && source.rgba_ptr == frame.rgba.as_ptr() as usize
                && source.rgba_len == frame.rgba.len()
        });
        if unchanged {
            return;
        }
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("vivido.vivid.source"),
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
                straight_alpha: u32::from(
                    frame.alpha_mode == vivid_protocol::messages::ALPHA_STRAIGHT,
                ),
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
        self.sources.insert(
            key,
            SourceTexture {
                _texture: texture,
                _options: options,
                bind_group,
                frame_id: frame.frame_id,
                pts_us: frame.pts_us,
                width: frame.width,
                height: frame.height,
                rgba_ptr: frame.rgba.as_ptr() as usize,
                rgba_len: frame.rgba.len(),
            },
        );
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
    }
}

fn vertices(item: &RenderItem, size: &SizeInfo, display_offset: usize) -> [Vertex; 6] {
    let x = size.padding_x() + fixed_to_f32(item.x) * size.cell_width();
    let scroll = if item.text_anchored { display_offset as f32 } else { 0.0 };
    let y = size.padding_y() + (fixed_to_f32(item.y) + scroll) * size.cell_height();
    let output_width = fixed_to_f32(item.width) * size.cell_width();
    let output_height = fixed_to_f32(item.height) * size.cell_height();
    let display_width =
        item.frame.width as f32 * item.frame.sar_num as f32 / item.frame.sar_den.max(1) as f32;
    let scale =
        (output_width / display_width).min(output_height / item.frame.height as f32).max(0.0);
    let width = display_width * scale;
    let height = item.frame.height as f32 * scale;
    let x = x + (output_width - width) * 0.5;
    let y = y + (output_height - height) * 0.5;
    let left = x / size.width() * 2.0 - 1.0;
    let right = (x + width) / size.width() * 2.0 - 1.0;
    let top = 1.0 - y / size.height() * 2.0;
    let bottom = 1.0 - (y + height) / size.height() * 2.0;
    [
        Vertex { position: [left, top], tex_coord: [0.0, 0.0] },
        Vertex { position: [left, bottom], tex_coord: [0.0, 1.0] },
        Vertex { position: [right, top], tex_coord: [1.0, 0.0] },
        Vertex { position: [right, top], tex_coord: [1.0, 0.0] },
        Vertex { position: [left, bottom], tex_coord: [0.0, 1.0] },
        Vertex { position: [right, bottom], tex_coord: [1.0, 1.0] },
    ]
}

fn fixed_to_f32(value: i64) -> f32 {
    (value as f64 / 4_294_967_296.0) as f32
}

#[cfg(test)]
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
    use vivid_protocol::messages::RasterSourceConfig;

    #[cfg(unix)]
    use crate::display::SizeInfo;
    #[cfg(unix)]
    use crate::vivid::scene::{Frame, SceneMutation, SceneNode, SharedScene, SourceConfig};

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
        let scene = SharedScene::default();
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
        let scene = SharedScene::default();
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
