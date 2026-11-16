//! Opt-in, bounded samples from real ordinary-window checked presentations.

// Rust guideline compliant 2026-10-07
use super::SceneRenderer;
use serde::Serialize;
use std::sync::mpsc::{self, Receiver, TryRecvError};

// The finite native fixture advances eight peer revisions. Two separately
// aligned one-pixel copies cost 512 staging bytes per frame, at most 4 KiB total.
const FRAMES: usize = 8;
const STRIDE: u64 = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT as u64;

#[derive(Debug, Serialize)]
struct Row {
    revision: u64,
    overlay_generation: u64,
    native_frame: u64,
    points: [[u32; 2]; 2],
    during_diagnosis: bool,
    input_requested: bool,
    pixels: Option<[[u8; 4]; 2]>,
}
struct Pending {
    row: Row,
    receiver: Receiver<Result<[[u8; 4]; 2], String>>,
}
pub(super) struct Probe {
    native_frames: u64,
    seen: Vec<u64>,
    pending: Vec<Pending>,
    rows: Vec<Row>,
    error: Option<String>,
    input_revision: Option<u64>,
}
impl Probe {
    pub(super) fn from_env() -> Option<Self> {
        (std::env::var_os("VIVIDO_OVERLAY_STREAM_CHECK").as_deref() == Some("1".as_ref())).then(
            || Self {
                native_frames: 0,
                seen: Vec::with_capacity(FRAMES),
                pending: Vec::with_capacity(FRAMES),
                rows: Vec::with_capacity(FRAMES),
                error: None,
                input_revision: None,
            },
        )
    }
    fn poll(&mut self) {
        let mut index = 0;
        while index < self.pending.len() {
            match self.pending[index].receiver.try_recv() {
                Ok(Ok(pixels)) => {
                    let mut completed = self.pending.remove(index).row;
                    completed.pixels = Some(pixels);
                    self.rows.push(completed);
                },
                Ok(Err(error)) => {
                    self.error = Some(error);
                    self.pending.remove(index);
                },
                Err(TryRecvError::Disconnected) => {
                    self.error = Some("native stream pixel callback was lost".into());
                    self.pending.remove(index);
                },
                Err(TryRecvError::Empty) => index += 1,
            }
        }
    }
    fn record(&mut self, renderer: &SceneRenderer) {
        self.native_frames = self.native_frames.saturating_add(1);
        self.poll();
        if self.seen.len() >= FRAMES || self.error.is_some() {
            return;
        }
        let (diagnosing, _, _) = renderer.overlays.stream_state_for_check();
        let Some((revision, generation, points)) = renderer.overlays.stream_frame_for_check()
        else {
            return;
        };
        // Revision two is the focused warm-up image. Sampling never creates a
        // native frame, waits for the GPU, or delays the private diagnosis.
        if !diagnosing || !(3..=10).contains(&revision) || self.seen.contains(&revision) {
            return;
        }
        if points
            .iter()
            .any(|[x, y]| *x >= renderer.target_size.width || *y >= renderer.target_size.height)
        {
            self.error = Some("native stream samples exceed the current target".into());
            return;
        }
        if self.input_revision.is_some() {
            self.error = Some("native stream input was not consumed by its draw".into());
            return;
        }
        self.input_revision = Some(revision);
        let row = Row {
            revision,
            overlay_generation: generation,
            native_frame: self.native_frames,
            points,
            during_diagnosis: diagnosing,
            input_requested: true,
            pixels: None,
        };
        let buffer = renderer.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("Vivido.overlay_stream_check.pixels"),
            size: STRIDE * 2,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = renderer.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("Vivido.overlay_stream_check.copy"),
        });
        for (index, [x, y]) in points.into_iter().enumerate() {
            encoder.copy_texture_to_buffer(
                wgpu::TexelCopyTextureInfo {
                    texture: &renderer.render_target,
                    mip_level: 0,
                    origin: wgpu::Origin3d { x, y, z: 0 },
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::TexelCopyBufferInfo {
                    buffer: &buffer,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: index as u64 * STRIDE,
                        bytes_per_row: Some(STRIDE as u32),
                        rows_per_image: Some(1),
                    },
                },
                wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
            );
        }
        renderer.queue.submit([encoder.finish()]);
        let (sender, receiver) = mpsc::sync_channel(1);
        let callback_buffer = buffer.clone();
        buffer.slice(..).map_async(wgpu::MapMode::Read, move |result| {
            let result = result.map_err(|e| e.to_string()).and_then(|()| {
                callback_buffer
                    .slice(..)
                    .get_mapped_range()
                    .map(|bytes| {
                        [
                            bytes[..4].try_into().expect("first bounded sample"),
                            bytes[STRIDE as usize..STRIDE as usize + 4]
                                .try_into()
                                .expect("second bounded sample"),
                        ]
                    })
                    .map_err(|e| e.to_string())
            });
            callback_buffer.unmap();
            let _ = sender.try_send(result);
        });
        self.seen.push(revision);
        self.pending.push(Pending { row, receiver });
    }
}
impl SceneRenderer {
    pub(super) fn record_overlay_stream_for_check(&self) {
        if let Some(probe) = self.stream_check.borrow_mut().as_mut() {
            probe.record(self);
        }
    }
    pub(crate) fn take_overlay_stream_input_for_check(&self) -> Option<u64> {
        self.stream_check.borrow_mut().as_mut()?.input_revision.take()
    }
    pub(crate) fn overlay_stream_for_check(&self) -> serde_json::Value {
        let mut guard = self.stream_check.borrow_mut();
        let (diagnosing, admitted, spent) = self.overlays.stream_state_for_check();
        let Some(probe) = guard.as_mut() else {
            return serde_json::json!({
                "enabled": false,
                "diagnosing": diagnosing,
                "worker_admitted": admitted,
                "shared_attempts": spent,
                "probe_passes": self.overlays.probe_passes_for_check(),
                "hidden_released": self.hidden_released,
                "target_size": [self.target_size.width, self.target_size.height],
            });
        };
        if let Err(error) = self.device.poll(wgpu::PollType::Poll) {
            probe.error = Some(error.to_string());
        }
        probe.poll();
        let (diagnosing, admitted, spent) = self.overlays.stream_state_for_check();
        serde_json::json!({
            "enabled": true,
            "probe_passes": self.overlays.probe_passes_for_check(),
            "hidden_released": self.hidden_released,
            "target_size": [self.target_size.width, self.target_size.height],
            "diagnosing": diagnosing,
            "worker_admitted": admitted,
            "shared_attempts": spent,
            "pending": probe.pending.len(),
            "rows": probe.rows,
            "error": probe.error,
            "max_rows": FRAMES,
            "staging_bytes_per_row": STRIDE * 2,
        })
    }
}
