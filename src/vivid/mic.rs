//! Consent, device capture, and bounded microphone egress. No device I/O runs on the UI thread.
use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample};
use vivid_protocol::audio_input::{InputPacket, PCM_BYTES, SAMPLES};
use vivid_protocol::identity::TrackIdentity;
use vivid_protocol::revision::ChannelGeneration;
use vivid_sdk::AudioInputSender;

use super::{ServiceShared, lock};

type Key = (TrackIdentity, ChannelGeneration);
struct Route {
    enabled: Arc<AtomicBool>,
    title: String,
}
#[derive(Default)]
struct State {
    routes: BTreeMap<Key, Route>,
    selected: Option<Key>,
    error: Option<String>,
}
#[derive(Default)]
pub(crate) struct Microphone(Mutex<State>);

struct Registration {
    shared: Arc<ServiceShared>,
    key: Key,
}

impl Drop for Registration {
    fn drop(&mut self) {
        lock(&self.shared.scene.microphone().0).routes.remove(&self.key);
        // Keep selection pinned to this owner; a replacement generation starts muted.
        self.shared.request_frame_wake();
    }
}

impl Microphone {
    pub fn toggle(&self) {
        let mut state = lock(&self.0);
        if state.selected.is_none() {
            state.selected = state.routes.keys().next().copied();
        }
        if let Some(route) = state.selected.and_then(|key| state.routes.get(&key)) {
            route.enabled.fetch_xor(true, Ordering::AcqRel);
        }
        state.error = None;
    }

    pub fn next(&self) {
        let mut state = lock(&self.0);
        for route in state.routes.values() {
            route.enabled.store(false, Ordering::Release);
        }
        state.selected = state
            .routes
            .keys()
            .copied()
            .find(|key| state.selected.is_none_or(|selected| *key > selected))
            .or_else(|| state.routes.keys().next().copied());
        state.error = None;
    }

    pub fn label(&self) -> String {
        let state = lock(&self.0);
        if let Some(error) = &state.error {
            return format!("MIC unavailable: {error}");
        }
        let route = match state.selected {
            Some(key) => state.routes.get(&key),
            None => state.routes.values().next(),
        };
        match route {
            Some(route) => format!(
                "MIC {}: {}",
                if route.enabled.load(Ordering::Acquire) { "ON" } else { "OFF" },
                route.title
            ),
            None if state.selected.is_some() => "MIC OFF: recipient disconnected".into(),
            None => String::new(),
        }
    }
}

pub(super) fn serve(
    reader: &mut super::transport::Reader,
    writer: &Arc<super::transport::Writer>,
    shared: &Arc<ServiceShared>,
    identity: TrackIdentity,
    generation: ChannelGeneration,
) -> io::Result<()> {
    let status = shared
        .scene
        .track_status(identity)
        .ok_or_else(|| io::Error::other("microphone track disappeared"))?;
    reader.set_write_timeout(Duration::from_millis(200))?;
    let tx = writer.clone();
    let sender = Arc::new(AudioInputSender::new(
        &status.configuration,
        generation,
        move |kind, object, body| tx.write_record_sequenced(kind, object, body),
    )?);
    let enabled = Arc::new(AtomicBool::new(false));
    let stopped = Arc::new(AtomicBool::new(false));
    let key = (identity, generation);
    let title = shared
        .scene
        .surface_status(identity.surface)
        .map(|surface| {
            surface
                .definition
                .descriptor
                .title
                .chars()
                .filter(|c| !c.is_control())
                .take(48)
                .collect()
        })
        .unwrap_or_else(|| "remote microphone".into());
    {
        let mut state = lock(&shared.scene.microphone().0);
        state.routes.insert(key, Route { enabled: enabled.clone(), title });
        if state.selected.is_some_and(|(selected, _)| selected == identity) {
            state.selected = Some(key);
        }
    }
    shared.request_frame_wake();
    let _registration = Registration { shared: shared.clone(), key };
    let shutdown = reader.shutdown_handle()?;
    let worker = thread::Builder::new().name("vivid-microphone".into()).spawn({
        let shared = shared.clone();
        let sender = sender.clone();
        let stopped = stopped.clone();
        move || {
            let result = (|| {
                while !enabled.load(Ordering::Acquire) {
                    if stopped.load(Ordering::Acquire)
                        || shared
                            .scene
                            .track_status(identity)
                            .is_none_or(|s| s.state.channel_generation != generation)
                    {
                        return Ok(());
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                capture(&enabled, &stopped, &sender, &shared, identity, generation)
            })();
            enabled.store(false, Ordering::Release);
            if let Err(error) = result {
                lock(&shared.scene.microphone().0).error = Some(error.to_string());
            }
            let _ = sender.eos();
            shutdown.stop();
            shared.request_frame_wake();
        }
    })?;
    let result = (|| {
        while !stopped.load(Ordering::Acquire) {
            sender.grant(&reader.read_record(vivid_protocol::wire::ConnectionKind::Track)?)?;
        }
        Ok(())
    })();
    stopped.store(true, Ordering::Release);
    let _ = worker.join();
    result
}

type Samples = Arc<Mutex<VecDeque<(u64, f32)>>>;

fn input_stream<T: SizedSample + Copy>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    samples: Samples,
    failed: Arc<AtomicBool>,
) -> io::Result<cpal::Stream>
where
    f32: FromSample<T>,
{
    let channels = usize::from(config.channels);
    let limit = config.sample_rate as usize / 5;
    let mut position = 0_u64;
    device
        .build_input_stream(
            *config,
            move |data: &[T], _| {
                let Ok(mut queue) = samples.try_lock() else {
                    position = position.saturating_add((data.len() / channels) as u64);
                    return;
                };
                for frame in data.chunks_exact(channels) {
                    let mono =
                        frame.iter().map(|s| f32::from_sample(*s)).sum::<f32>() / channels as f32;
                    if queue.len() == limit {
                        queue.pop_front();
                    }
                    queue.push_back((
                        position,
                        if mono.is_finite() { mono.clamp(-1.0, 1.0) } else { 0.0 },
                    ));
                    position = position.saturating_add(1);
                }
            },
            move |_| {
                failed.store(true, Ordering::Release);
            },
            None,
        )
        .map_err(io::Error::other)
}

fn capture(
    enabled: &AtomicBool,
    stopped: &AtomicBool,
    sender: &AudioInputSender,
    shared: &ServiceShared,
    identity: TrackIdentity,
    generation: ChannelGeneration,
) -> io::Result<()> {
    let device = cpal::default_host()
        .default_input_device()
        .ok_or_else(|| io::Error::other("no microphone device"))?;
    let supported = device.default_input_config().map_err(io::Error::other)?;
    let config: cpal::StreamConfig = supported.into();
    if !(8_000..=192_000).contains(&config.sample_rate) || !(1..=8).contains(&config.channels) {
        return Err(io::Error::other("unsupported microphone rate or channel count"));
    }
    let queue = Arc::new(Mutex::new(VecDeque::with_capacity(config.sample_rate as usize / 5)));
    let failed = Arc::new(AtomicBool::new(false));
    let stream = match supported.sample_format() {
        SampleFormat::F32 => input_stream::<f32>(&device, &config, queue.clone(), failed.clone()),
        SampleFormat::I16 => input_stream::<i16>(&device, &config, queue.clone(), failed.clone()),
        SampleFormat::U16 => input_stream::<u16>(&device, &config, queue.clone(), failed.clone()),
        _ => Err(io::Error::other("unsupported microphone sample format")),
    }?;
    let mut resampler = super::audio::CaptureResampler::new(config.sample_rate)?;
    stream.play().map_err(io::Error::other)?;
    let mut expected = 0_u64;
    let mut pts = 0_i64;
    let mut last_pts = -20_000_i64;
    let mut pending: VecDeque<i16> = VecDeque::with_capacity(SAMPLES + 8192);
    let mut output = [0_i16; 8192];
    while enabled.load(Ordering::Acquire)
        && !stopped.load(Ordering::Acquire)
        && shared
            .scene
            .track_status(identity)
            .is_some_and(|s| s.state.channel_generation == generation)
    {
        if failed.load(Ordering::Acquire) {
            return Err(io::Error::other("microphone device disconnected"));
        }
        let samples: Vec<_> = {
            let mut queue = lock(&queue);
            let count = queue.len().min(1024);
            queue.drain(..count).collect()
        };
        if samples.is_empty() {
            thread::sleep(Duration::from_millis(5));
            continue;
        }
        if samples[0].0 != expected {
            resampler = super::audio::CaptureResampler::new(config.sample_rate)?;
            pending.clear();
            pts = i64::try_from(
                samples[0].0.saturating_mul(1_000_000) / u64::from(config.sample_rate),
            )
            .map_err(|_| io::Error::other("microphone clock exhausted"))?
            .max(last_pts + 20_000);
        }
        expected = samples.last().map_or(expected, |s| s.0.saturating_add(1));
        let input: Vec<_> = samples.into_iter().map(|s| s.1).collect();
        let count = resampler.convert(&input, &mut output)?;
        pending.extend(&output[..count]);
        while pending.len() >= SAMPLES {
            let mut pcm = [0; PCM_BYTES];
            for (bytes, sample) in pcm.chunks_exact_mut(2).zip(pending.drain(..SAMPLES)) {
                bytes.copy_from_slice(&sample.to_le_bytes());
            }
            if !enabled.load(Ordering::Acquire) || stopped.load(Ordering::Acquire) {
                break;
            }
            sender.try_send(&InputPacket { epoch: 1, packet_id: 1, pts_us: pts, pcm })?;
            last_pts = pts;
            pts = pts
                .checked_add(20_000)
                .ok_or_else(|| io::Error::other("microphone clock exhausted"))?;
        }
    }
    drop(stream);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use vivid_protocol::identity::{PresenterInstanceId, SessionIdentity};

    #[test]
    fn consent_is_exclusive_and_a_missing_owner_is_not_redirected() {
        let controller = Microphone::default();
        let mut keys = Vec::new();
        for owner in [7, 8] {
            let surface = SessionIdentity::new(PresenterInstanceId([1; 16]), owner)
                .unwrap()
                .context(1)
                .unwrap()
                .surface(9)
                .unwrap();
            let key = (TrackIdentity { surface, track_id: 11 }, ChannelGeneration::new(1));
            keys.push(key);
            lock(&controller.0).routes.insert(
                key,
                Route {
                    enabled: Arc::new(AtomicBool::new(false)),
                    title: format!("owner {owner}"),
                },
            );
        }
        assert!(controller.label().contains("OFF"));
        controller.toggle();
        assert!(lock(&controller.0).routes[&keys[0]].enabled.load(Ordering::Acquire));
        controller.next();
        assert!(
            lock(&controller.0).routes.values().all(|route| !route.enabled.load(Ordering::Acquire))
        );
        controller.toggle();
        assert!(lock(&controller.0).routes[&keys[1]].enabled.load(Ordering::Acquire));
        lock(&controller.0).routes.remove(&keys[1]);
        controller.toggle();
        assert_eq!(controller.label(), "MIC OFF: recipient disconnected");
        assert!(!lock(&controller.0).routes[&keys[0]].enabled.load(Ordering::Acquire));
    }

    #[test]
    fn capture_resampling_handles_noncanonical_device_rates_without_a_device() {
        for rate in [8_000, 44_100, 48_000, 96_000] {
            let mut resampler = super::super::audio::CaptureResampler::new(rate).unwrap();
            let input = vec![0.25_f32; rate as usize / 10];
            let mut output = [0_i16; 6000];
            let count = resampler.convert(&input, &mut output).unwrap();
            assert!((4600..=4800).contains(&count), "{rate}: {count}");
            assert!(output[..count].iter().any(|sample| *sample > 7000));
        }
    }
}
