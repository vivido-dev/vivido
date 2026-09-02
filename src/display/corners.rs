//! Rounded window corners for client-side decorated surfaces.
//!
//! A Vivido chrome window draws its own frame, so nothing else rounds it: Mutter and the other
//! Wayland compositors never decorate it, and a square-cornered window sits visibly apart from
//! every other window on the desktop. The rounding cannot live in the Vello scene, because the
//! terminal pane arrives as a raw texture copy over the top of that scene, so it is applied to
//! the finished render target instead by clearing the alpha outside a rounded rectangle. Vello
//! writes straight alpha, so only the alpha channel is touched and the colour beneath the arc is
//! left exactly as it was.

use bytemuck::{Pod, Zeroable};
use vello::wgpu;
use winit::dpi::PhysicalSize;

const SHADER: &str = r#"
struct Corners {
    size: vec2<f32>,
    radius: f32,
    _padding: f32,
};

@group(0) @binding(0) var<uniform> corners: Corners;

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    // One oversized triangle covers clip space; the scissor rectangle picks the corner.
    let x = f32(i32(index & 1u) * 4 - 1);
    let y = f32(i32(index >> 1u) * 4 - 1);
    return vec4<f32>(x, y, 0.0, 1.0);
}

@fragment
fn fs_main(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let half_size = corners.size * 0.5;
    let inner = half_size - vec2<f32>(corners.radius, corners.radius);
    let offset = max(abs(position.xy - half_size) - inner, vec2<f32>(0.0, 0.0));
    // Signed distance to the rounded rectangle, spread over one pixel so the arc is antialiased.
    let outside = clamp(length(offset) - corners.radius + 0.5, 0.0, 1.0);
    return vec4<f32>(0.0, 0.0, 0.0, outside);
}
"#;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Corners {
    size: [f32; 2],
    radius: f32,
    _padding: f32,
}

/// Alpha-only pass which rounds the corners of a finished render target.
pub struct CornerMask {
    pipeline: wgpu::RenderPipeline,
    uniform: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
}

impl CornerMask {
    pub fn new(device: &wgpu::Device) -> Self {
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("vivido.corners.bind_group_layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("vivido.corners.shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("vivido.corners.pipeline_layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("vivido.corners.pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    // Keep the destination colour and scale only its straight alpha by the
                    // coverage outside the rounded rectangle.
                    blend: Some(wgpu::BlendState {
                        color: wgpu::BlendComponent {
                            src_factor: wgpu::BlendFactor::Zero,
                            dst_factor: wgpu::BlendFactor::One,
                            operation: wgpu::BlendOperation::Add,
                        },
                        alpha: wgpu::BlendComponent {
                            src_factor: wgpu::BlendFactor::Zero,
                            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                            operation: wgpu::BlendOperation::Add,
                        },
                    }),
                    write_mask: wgpu::ColorWrites::ALPHA,
                })],
            }),
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            multiview_mask: None,
            cache: None,
        });
        let uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("vivido.corners.uniform"),
            size: std::mem::size_of::<Corners>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("vivido.corners.bind_group"),
            layout: &bind_group_layout,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: uniform.as_entire_binding() }],
        });

        Self { pipeline, uniform, bind_group }
    }

    /// Clear the alpha outside a `radius`-rounded rectangle covering the whole target.
    pub fn apply(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        view: &wgpu::TextureView,
        size: PhysicalSize<u32>,
        radius: f32,
    ) {
        let Some((extent, origins)) = corner_scissors(size, radius) else {
            return;
        };
        queue.write_buffer(
            &self.uniform,
            0,
            bytemuck::bytes_of(&Corners {
                size: [size.width as f32, size.height as f32],
                radius,
                _padding: 0.0,
            }),
        );

        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("vivido.corners.encoder"),
        });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("vivido.corners.pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            for (x, y) in origins {
                pass.set_scissor_rect(x, y, extent, extent);
                pass.draw(0..3, 0..1);
            }
        }
        queue.submit([encoder.finish()]);
    }
}

/// Square of pixels each corner arc can touch, with the four scissor origins.
///
/// Only those squares are rasterized: the interior of the window is untouched by the pass, and a
/// radius wider than half the window would otherwise let opposite corners overlap.
fn corner_scissors(size: PhysicalSize<u32>, radius: f32) -> Option<(u32, [(u32, u32); 4])> {
    if !radius.is_finite() || radius <= 0.0 || size.width == 0 || size.height == 0 {
        return None;
    }
    let limit = size.width.min(size.height) / 2;
    let extent = (radius.ceil() as u32).min(limit);
    if extent == 0 {
        return None;
    }
    Some((
        extent,
        [
            (0, 0),
            (size.width - extent, 0),
            (0, size.height - extent),
            (size.width - extent, size.height - extent),
        ],
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corner_squares_stay_inside_the_target() {
        let (extent, origins) = corner_scissors(PhysicalSize::new(800, 600), 12.0).unwrap();
        assert_eq!(extent, 12);
        assert_eq!(origins, [(0, 0), (788, 0), (0, 588), (788, 588)]);
    }

    #[test]
    fn corner_squares_never_overlap_on_a_small_target() {
        let (extent, origins) = corner_scissors(PhysicalSize::new(20, 10), 12.0).unwrap();
        assert_eq!(extent, 5);
        assert_eq!(origins, [(0, 0), (15, 0), (0, 5), (15, 5)]);
    }

    #[test]
    fn degenerate_geometry_skips_the_pass() {
        assert!(corner_scissors(PhysicalSize::new(0, 600), 12.0).is_none());
        assert!(corner_scissors(PhysicalSize::new(800, 0), 12.0).is_none());
        assert!(corner_scissors(PhysicalSize::new(800, 600), 0.0).is_none());
        assert!(corner_scissors(PhysicalSize::new(800, 600), f32::NAN).is_none());
        assert!(corner_scissors(PhysicalSize::new(800, 1), 12.0).is_none());
    }
}
