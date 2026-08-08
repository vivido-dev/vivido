use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::io;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, I24, SampleFormat, SizedSample, Stream, StreamConfig, U24};
use ringbuf::traits::{Consumer, Producer, Split};
use ringbuf::{HeapProd, HeapRb};
use vivid_protocol::media::ParsedAudioPacket;
use vivid_protocol::track::AudioConfiguration;

use crate::vivid::ffmpeg::{self, AVPacket, AVRational, ParameterValues};

const AVMEDIA_TYPE_AUDIO: c_int = 1;
const AV_INPUT_BUFFER_PADDING_SIZE: usize = 64;
const AV_SAMPLE_FMT_FLT: c_int = 3;
const AVERROR_EOF: c_int = -541_478_725;
const PACKET_TIME_BASE: AVRational = AVRational { num: 1, den: 1_000_000 };
// Remote audio commonly arrives in bursts after SSH scheduling or network jitter. Keep a bounded
// reserve large enough to bridge those stalls; linked video is discarded when it falls behind.
const RING_BUFFER_SECONDS: usize = 2;
const PREBUFFER_MILLISECONDS: u64 = 100;
/// The most a live stream will be delayed to line sound up with picture. Past this the session is
/// no longer interactive, and the honest answer is a lower encoder target, not more buffering.
///
/// Held audio accumulates in the device ring, so this must stay comfortably under
/// [`RING_BUFFER_SECONDS`]: steady-state ring occupancy is the delay itself, and a delay that
/// could reach the ring's capacity would block the decoder thread pushing into it.
const MAXIMUM_LIVE_DELAY_US: u64 = 1_000_000;
/// Delay changes smaller than this are transport jitter, not skew worth re-buffering for.
const LIVE_DELAY_STEP_US: u64 = 20_000;
const LINKED_AUDIO_STALL_FALLBACK: Duration = Duration::from_secs(2);
/// How much of a full ring buffer to wait for the device to drain before retrying a push.
const RING_WAIT_SAMPLES: u64 = 512;
/// A floor on that wait, so a very high sample rate cannot turn it back into a spin.
const MINIMUM_RING_WAIT: Duration = Duration::from_micros(500);
/// How often to re-check an output that is not draining, so `stop` and errors are still noticed.
const STOPPED_OUTPUT_RECHECK: Duration = Duration::from_millis(5);
const UNSET_PTS: i64 = i64::MIN;

pub fn supports(config: &AudioConfiguration) -> bool {
    AudioDecoder::new(config, config.sample_rate, u16::from(config.channels)).is_ok()
}

#[repr(C)]
#[derive(Default)]
struct AVChannelLayout {
    order: c_int,
    nb_channels: c_int,
    mask: u64,
    opaque: *mut c_void,
}

struct Shared {
    enabled: AtomicBool,
    prebuffered: AtomicBool,
    stopped: AtomicBool,
    decode_done: AtomicBool,
    eos_observed: AtomicBool,
    queued_samples: AtomicU64,
    played_samples: AtomicU64,
    discard_samples: AtomicU64,
    rendered_samples: AtomicU64,
    timeline_origin_us: AtomicI64,
    first_audio_pts_us: AtomicI64,
    leading_silence_samples: AtomicU64,
    /// Silence the device emits *without* advancing the media clock or consuming the ring.
    ///
    /// This is the live audio/video presentation delay. Leading silence is the opposite tool: it
    /// covers timeline that has already elapsed and therefore does advance the clock.
    hold_silence_samples: AtomicU64,
    /// Silence covering timeline the producer dropped. It advances the media clock, like leading
    /// silence, but has its own counter because `observe_timeline_pts` recomputes and *stores*
    /// leading silence on every packet and would otherwise erase a bridge before it played out.
    gap_silence_samples: AtomicU64,
    /// The delay the hold is currently implementing, so it can be measured and adjusted.
    live_delay_us: AtomicU64,
    prebuffer_samples: AtomicU64,
    requested_start_pts_us: AtomicI64,
    play_configured_at: Mutex<Option<Instant>>,
    clock_progress: Mutex<ClockProgress>,
    error: Mutex<Option<String>>,
}

struct ClockProgress {
    rendered_samples: u64,
    observed_at: Instant,
}

impl Shared {
    fn set_error(&self, message: impl Into<String>) {
        let mut error = self.error.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if error.is_none() {
            *error = Some(message.into());
        }
    }

    fn error(&self) -> Option<String> {
        self.error.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
    }
}

pub struct AudioOutput {
    shared: Arc<Shared>,
    producer: Mutex<HeapProd<f32>>,
    _stream: Option<Stream>,
    sample_rate: u32,
    channels: u16,
}

impl AudioOutput {
    pub fn open() -> io::Result<Arc<Self>> {
        let host = cpal::default_host();
        let device = host.default_output_device().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "no default audio output device")
        })?;
        let supported = device.default_output_config().map_err(|error| {
            io::Error::other(format!("could not query the default audio output: {error}"))
        })?;
        let format = supported.sample_format();
        let config: StreamConfig = supported.into();
        let shared = Arc::new(Shared {
            enabled: AtomicBool::new(false),
            prebuffered: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            decode_done: AtomicBool::new(false),
            eos_observed: AtomicBool::new(false),
            queued_samples: AtomicU64::new(0),
            played_samples: AtomicU64::new(0),
            discard_samples: AtomicU64::new(0),
            rendered_samples: AtomicU64::new(0),
            timeline_origin_us: AtomicI64::new(UNSET_PTS),
            first_audio_pts_us: AtomicI64::new(UNSET_PTS),
            leading_silence_samples: AtomicU64::new(0),
            hold_silence_samples: AtomicU64::new(0),
            gap_silence_samples: AtomicU64::new(0),
            live_delay_us: AtomicU64::new(0),
            prebuffer_samples: AtomicU64::new(
                u64::from(config.sample_rate)
                    .saturating_mul(u64::from(config.channels))
                    .saturating_mul(PREBUFFER_MILLISECONDS)
                    / 1_000,
            ),
            requested_start_pts_us: AtomicI64::new(UNSET_PTS),
            play_configured_at: Mutex::new(None),
            clock_progress: Mutex::new(ClockProgress {
                rendered_samples: 0,
                observed_at: Instant::now(),
            }),
            error: Mutex::new(None),
        });
        let ring = HeapRb::<f32>::new(
            (config.sample_rate as usize * config.channels as usize * RING_BUFFER_SECONDS).max(1),
        );
        let (producer, consumer) = ring.split();
        let stream = match format {
            SampleFormat::I8 => build_stream::<i8, _>(&device, &config, consumer, shared.clone()),
            SampleFormat::I16 => build_stream::<i16, _>(&device, &config, consumer, shared.clone()),
            SampleFormat::I24 => build_stream::<I24, _>(&device, &config, consumer, shared.clone()),
            SampleFormat::I32 => build_stream::<i32, _>(&device, &config, consumer, shared.clone()),
            SampleFormat::I64 => build_stream::<i64, _>(&device, &config, consumer, shared.clone()),
            SampleFormat::U8 => build_stream::<u8, _>(&device, &config, consumer, shared.clone()),
            SampleFormat::U16 => build_stream::<u16, _>(&device, &config, consumer, shared.clone()),
            SampleFormat::U24 => build_stream::<U24, _>(&device, &config, consumer, shared.clone()),
            SampleFormat::U32 => build_stream::<u32, _>(&device, &config, consumer, shared.clone()),
            SampleFormat::U64 => build_stream::<u64, _>(&device, &config, consumer, shared.clone()),
            SampleFormat::F32 => build_stream::<f32, _>(&device, &config, consumer, shared.clone()),
            SampleFormat::F64 => build_stream::<f64, _>(&device, &config, consumer, shared.clone()),
            unsupported => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("unsupported audio sample format {unsupported}"),
            )),
        }?;
        stream
            .play()
            .map_err(|error| io::Error::other(format!("could not start audio output: {error}")))?;
        Ok(Arc::new(Self {
            shared,
            producer: Mutex::new(producer),
            _stream: Some(stream),
            sample_rate: config.sample_rate,
            channels: config.channels,
        }))
    }

    #[cfg(test)]
    pub(super) fn test_output() -> Arc<Self> {
        let shared = Arc::new(Shared {
            enabled: AtomicBool::new(false),
            prebuffered: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            decode_done: AtomicBool::new(false),
            eos_observed: AtomicBool::new(false),
            queued_samples: AtomicU64::new(0),
            played_samples: AtomicU64::new(0),
            discard_samples: AtomicU64::new(0),
            rendered_samples: AtomicU64::new(0),
            timeline_origin_us: AtomicI64::new(UNSET_PTS),
            first_audio_pts_us: AtomicI64::new(UNSET_PTS),
            leading_silence_samples: AtomicU64::new(0),
            hold_silence_samples: AtomicU64::new(0),
            gap_silence_samples: AtomicU64::new(0),
            live_delay_us: AtomicU64::new(0),
            prebuffer_samples: AtomicU64::new(0),
            requested_start_pts_us: AtomicI64::new(UNSET_PTS),
            play_configured_at: Mutex::new(None),
            clock_progress: Mutex::new(ClockProgress {
                rendered_samples: 0,
                observed_at: Instant::now(),
            }),
            error: Mutex::new(None),
        });
        let (producer, _consumer) = HeapRb::<f32>::new(32).split();
        Arc::new(Self {
            shared,
            producer: Mutex::new(producer),
            _stream: None,
            sample_rate: 48_000,
            channels: 2,
        })
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(super) fn force_video_gate_stall_for_test(&self) {
        *self.shared.play_configured_at.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some(Instant::now() - LINKED_AUDIO_STALL_FALLBACK - Duration::from_millis(1));
        self.shared
            .clock_progress
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .observed_at = Instant::now() - LINKED_AUDIO_STALL_FALLBACK - Duration::from_millis(1);
    }

    pub fn decoder(&self, config: &AudioConfiguration) -> io::Result<AudioDecoder> {
        AudioDecoder::new(config, self.sample_rate, self.channels)
    }

    pub fn start(&self) {
        self.shared.enabled.store(true, Ordering::SeqCst);
    }

    pub fn configure_play(&self, start_pts_us: i64, minimum_buffer_us: u64) {
        self.shared.requested_start_pts_us.store(start_pts_us, Ordering::SeqCst);
        self.shared.timeline_origin_us.store(start_pts_us, Ordering::SeqCst);
        self.shared.prebuffer_samples.store(
            u64::from(self.sample_rate)
                .saturating_mul(u64::from(self.channels))
                .saturating_mul(minimum_buffer_us)
                / 1_000_000,
            Ordering::SeqCst,
        );
        self.shared.prebuffered.store(false, Ordering::SeqCst);
        *self.shared.play_configured_at.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some(Instant::now());
        *self.shared.clock_progress.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) =
            ClockProgress {
                rendered_samples: self.shared.rendered_samples.load(Ordering::SeqCst),
                observed_at: Instant::now(),
            };
    }

    pub fn pause(&self) {
        self.shared.enabled.store(false, Ordering::SeqCst);
    }

    pub fn flush(&self) {
        self.pause();
        let outstanding = self
            .shared
            .queued_samples
            .load(Ordering::SeqCst)
            .saturating_sub(self.shared.played_samples.load(Ordering::SeqCst));
        self.shared.discard_samples.store(outstanding, Ordering::SeqCst);
        self.shared.hold_silence_samples.store(0, Ordering::SeqCst);
        self.shared.gap_silence_samples.store(0, Ordering::SeqCst);
        self.shared.live_delay_us.store(0, Ordering::SeqCst);
        self.shared.prebuffered.store(false, Ordering::SeqCst);
        self.shared.decode_done.store(false, Ordering::SeqCst);
        self.shared.eos_observed.store(false, Ordering::SeqCst);
        self.shared.rendered_samples.store(0, Ordering::SeqCst);
        self.shared.timeline_origin_us.store(UNSET_PTS, Ordering::SeqCst);
        self.shared.requested_start_pts_us.store(UNSET_PTS, Ordering::SeqCst);
        self.shared.first_audio_pts_us.store(UNSET_PTS, Ordering::SeqCst);
        self.shared.leading_silence_samples.store(0, Ordering::SeqCst);
        *self.shared.play_configured_at.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) =
            None;
        *self.shared.clock_progress.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) =
            ClockProgress { rendered_samples: 0, observed_at: Instant::now() };
    }

    fn observe_timeline_pts(&self, pts_us: i64) {
        if pts_us == UNSET_PTS {
            return;
        }
        if self.shared.requested_start_pts_us.load(Ordering::SeqCst) != UNSET_PTS {
            return;
        }
        let mut current = self.shared.timeline_origin_us.load(Ordering::SeqCst);
        while current == UNSET_PTS || pts_us < current {
            match self.shared.timeline_origin_us.compare_exchange(
                current,
                pts_us,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
        let origin = self.shared.timeline_origin_us.load(Ordering::SeqCst);
        let audio_pts = self.shared.first_audio_pts_us.load(Ordering::SeqCst);
        if origin != UNSET_PTS && audio_pts != UNSET_PTS {
            let desired_samples =
                leading_silence_sample_count(origin, audio_pts, self.sample_rate, self.channels);
            let rendered = self.shared.rendered_samples.load(Ordering::SeqCst);
            self.shared
                .leading_silence_samples
                .store(desired_samples.saturating_sub(rendered), Ordering::SeqCst);
        }
    }

    pub fn observe_audio_pts(&self, pts_us: i64) {
        if pts_us != UNSET_PTS {
            let _ = self.shared.first_audio_pts_us.compare_exchange(
                UNSET_PTS,
                pts_us,
                Ordering::SeqCst,
                Ordering::SeqCst,
            );
        }
        self.observe_timeline_pts(pts_us);
    }

    pub fn trim_before_start(&self, pts_us: i64, duration_us: u64, samples: &mut Vec<f32>) {
        let start = self.shared.requested_start_pts_us.load(Ordering::SeqCst);
        if start == UNSET_PTS || pts_us >= start || samples.is_empty() {
            return;
        }
        let packet_end = pts_us.saturating_add(i64::try_from(duration_us).unwrap_or(i64::MAX));
        if packet_end <= start || duration_us == 0 {
            samples.clear();
            return;
        }
        let discard_us = start.saturating_sub(pts_us) as u64;
        let discard = u64::from(self.sample_rate)
            .saturating_mul(u64::from(self.channels))
            .saturating_mul(discard_us)
            / 1_000_000;
        let discard = usize::try_from(discard).unwrap_or(usize::MAX).min(samples.len());
        samples.drain(..discard);
    }

    pub fn pts_reached(&self, pts_us: i64) -> bool {
        if !self.shared.enabled.load(Ordering::SeqCst)
            || !self.shared.prebuffered.load(Ordering::SeqCst)
        {
            return false;
        }
        let origin = self.shared.timeline_origin_us.load(Ordering::SeqCst);
        origin != UNSET_PTS
            && pts_us
                <= rendered_pts_us(
                    origin,
                    self.shared.rendered_samples.load(Ordering::SeqCst),
                    self.sample_rate,
                    self.channels,
                )
    }

    /// The media PTS currently leaving the device, or `None` before playback is running.
    pub fn rendered_pts(&self) -> Option<i64> {
        if !self.shared.enabled.load(Ordering::SeqCst)
            || !self.shared.prebuffered.load(Ordering::SeqCst)
        {
            return None;
        }
        let origin = self.shared.timeline_origin_us.load(Ordering::SeqCst);
        (origin != UNSET_PTS).then(|| {
            rendered_pts_us(
                origin,
                self.shared.rendered_samples.load(Ordering::SeqCst),
                self.sample_rate,
                self.channels,
            )
        })
    }

    /// Oldest video PTS worth converting for a linked live stream. Frames before this threshold
    /// are still decoded as references, but their pixels need not be allocated or uploaded.
    ///
    /// The threshold is measured against the clock *after* the live presentation delay, so it means
    /// "older than the picture that belongs with the sound coming out of the speaker" rather than
    /// "slower to arrive than audio". Those are the same thing only on a link with no video
    /// queueing delay, and treating them as the same discarded every frame of a congested session.
    pub fn discard_video_before(&self, maximum_latency_us: u64) -> Option<i64> {
        self.rendered_pts()
            .map(|pts| pts.saturating_sub(i64::try_from(maximum_latency_us).unwrap_or(i64::MAX)))
    }

    /// How far behind capture this output is deliberately playing, in microseconds.
    pub fn live_delay_us(&self) -> u64 {
        self.shared.live_delay_us.load(Ordering::SeqCst)
    }

    /// Hold audio back so it reaches the speaker when the picture it belongs with reaches the
    /// screen.
    ///
    /// Audio and video are captured on one clock but do not arrive on one: video is two orders of
    /// magnitude larger and queues behind itself in the transport. Playing audio the moment it
    /// arrives therefore runs sound ahead of picture by exactly that difference. The device holds
    /// the difference as silence that does not advance the media clock, which is the only way the
    /// two can be presented together. Returns the delay now in force.
    pub fn request_live_delay(&self, target_us: u64) -> u64 {
        let target_us = target_us.min(MAXIMUM_LIVE_DELAY_US);
        let current = self.shared.live_delay_us.load(Ordering::SeqCst);
        if target_us.abs_diff(current) < LIVE_DELAY_STEP_US {
            return current;
        }
        if target_us > current {
            let added = target_us - current;
            self.shared.hold_silence_samples.fetch_add(self.samples_for(added), Ordering::SeqCst);
            self.shared.live_delay_us.store(target_us, Ordering::SeqCst);
            return target_us;
        }
        // Shrinking the delay means throwing away audio that has already been buffered, so it is
        // only worth doing when the saving is audible-scale and the excess is durable.
        let removed = current - target_us;
        self.shared.discard_samples.fetch_add(self.samples_for(removed), Ordering::SeqCst);
        self.shared.live_delay_us.store(target_us, Ordering::SeqCst);
        target_us
    }

    /// How long until the media clock reaches `pts_us`, or `None` when it already has.
    ///
    /// The device renders at a fixed rate, so the remaining time is arithmetic rather than
    /// something to poll for. `None` is also returned when the clock is not running: the caller
    /// has its own fallback for that and must not wait on a clock that is not advancing.
    pub fn time_until_pts(&self, pts_us: i64) -> Option<Duration> {
        let rendered = self.rendered_pts()?;
        let remaining = pts_us.checked_sub(rendered).filter(|remaining| *remaining > 0)?;
        Some(Duration::from_micros(u64::try_from(remaining).unwrap_or(u64::MAX)))
    }

    /// How long until the device has consumed `samples` from the ring.
    ///
    /// Used to wait out a full ring buffer without polling it. The device only drains while it is
    /// enabled, so a stopped or paused output reports nothing and the caller re-checks its own
    /// exit conditions instead.
    fn time_to_drain(&self, samples: u64) -> Option<Duration> {
        if !self.shared.enabled.load(Ordering::SeqCst) {
            return None;
        }
        let rate = u64::from(self.sample_rate).saturating_mul(u64::from(self.channels));
        (rate > 0)
            .then(|| Duration::from_micros(samples.saturating_mul(1_000_000).div_ceil(rate.max(1))))
    }

    fn samples_for(&self, duration_us: u64) -> u64 {
        u64::from(self.sample_rate)
            .saturating_mul(u64::from(self.channels))
            .saturating_mul(duration_us)
            / 1_000_000
    }

    /// Cover a gap in the encoded audio timeline without interrupting playback.
    ///
    /// A producer that dropped packets leaves a hole. Silence of exactly the hole's length keeps
    /// the media clock honest — the alternative, restarting the timeline, discards every buffered
    /// sample and is audible every time transport jitter is mistaken for a discontinuity.
    pub fn bridge_live_gap(&self, gap_us: u64) {
        self.shared.gap_silence_samples.fetch_add(self.samples_for(gap_us), Ordering::SeqCst);
    }

    /// Drop queued device samples and restart a live timeline. Reserved for a producer that
    /// genuinely restarted its clock, which is the only case a continuous timeline cannot express.
    pub fn rebase_live(&self, pts_us: i64) {
        self.flush();
        self.observe_audio_pts(pts_us);
        self.start();
    }

    pub fn video_gate_stalled(&self) -> bool {
        if !self.shared.enabled.load(Ordering::SeqCst)
            || !self
                .shared
                .play_configured_at
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_some_and(|configured| configured.elapsed() > LINKED_AUDIO_STALL_FALLBACK)
        {
            return false;
        }
        let rendered_samples = self.shared.rendered_samples.load(Ordering::SeqCst);
        let mut progress =
            self.shared.clock_progress.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if rendered_samples != progress.rendered_samples {
            progress.rendered_samples = rendered_samples;
            progress.observed_at = Instant::now();
            return false;
        }
        if progress.observed_at.elapsed() <= LINKED_AUDIO_STALL_FALLBACK {
            return false;
        }
        drop(progress);
        self.shared.set_error("linked audio output clock stalled");
        true
    }

    pub fn push(&self, samples: &[f32]) -> io::Result<()> {
        let mut producer = self.producer.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        for &sample in samples {
            let mut sample = sample;
            loop {
                if self.shared.stopped.load(Ordering::SeqCst) {
                    return Ok(());
                }
                if let Some(error) = self.shared.error() {
                    return Err(io::Error::new(io::ErrorKind::NotConnected, error));
                }
                match producer.try_push(sample) {
                    Ok(()) => {
                        self.shared.queued_samples.fetch_add(1, Ordering::SeqCst);
                        break;
                    },
                    Err(value) => {
                        sample = value;
                        // The ring is full. It drains at the device's fixed rate, so wait for the
                        // device to consume a chunk rather than polling it. Signalling from the
                        // audio callback instead would put a lock on the realtime thread.
                        let wait = self
                            .time_to_drain(RING_WAIT_SAMPLES)
                            .unwrap_or(STOPPED_OUTPUT_RECHECK)
                            .clamp(MINIMUM_RING_WAIT, STOPPED_OUTPUT_RECHECK);
                        thread::sleep(wait);
                    },
                }
            }
        }
        Ok(())
    }

    pub fn finish_decode(&self) {
        self.shared.decode_done.store(true, Ordering::SeqCst);
    }

    pub fn signal_eos(&self) {
        self.shared.eos_observed.store(true, Ordering::SeqCst);
    }

    /// One non-blocking drain check.
    ///
    /// `None` means still draining. Media §15 makes `DRAIN` bounded pending state that must not
    /// block control, so the session actor polls this instead of parking a thread on
    /// [`Self::wait_drained`].
    pub fn poll_drained(&self) -> Option<io::Result<()>> {
        if self.shared.stopped.load(Ordering::SeqCst) {
            return Some(Err(io::Error::new(io::ErrorKind::BrokenPipe, "audio output stopped")));
        }
        if let Some(error) = self.shared.error() {
            return Some(Err(io::Error::new(io::ErrorKind::NotConnected, error)));
        }
        let drained = self.shared.eos_observed.load(Ordering::SeqCst)
            && self.shared.decode_done.load(Ordering::SeqCst)
            && self.shared.played_samples.load(Ordering::SeqCst)
                >= self.shared.queued_samples.load(Ordering::SeqCst);
        drained.then_some(Ok(()))
    }

    pub fn stop(&self) {
        self.shared.stopped.store(true, Ordering::SeqCst);
        // Session teardown must silence the device immediately. The stopped flag unblocks
        // decoders and waiters; leaving enabled set would let the callback continue draining
        // already queued samples after the track and its surface nodes had been removed.
        self.shared.enabled.store(false, Ordering::SeqCst);
    }
}

fn build_stream<T, C>(
    device: &cpal::Device,
    config: &StreamConfig,
    mut consumer: C,
    shared: Arc<Shared>,
) -> io::Result<Stream>
where
    T: SizedSample + FromSample<f32>,
    C: Consumer<Item = f32> + Send + 'static,
{
    let error_shared = shared.clone();
    device
        .build_output_stream(
            *config,
            move |output: &mut [T], _| {
                let mut discarded = 0_u64;
                while discarded < output.len() as u64 {
                    if shared.discard_samples.load(Ordering::SeqCst) == 0 {
                        break;
                    }
                    if consumer.try_pop().is_none() {
                        break;
                    }
                    let _ = shared.discard_samples.fetch_update(
                        Ordering::SeqCst,
                        Ordering::SeqCst,
                        |remaining| remaining.checked_sub(1),
                    );
                    discarded += 1;
                }
                if discarded > 0 {
                    shared.played_samples.fetch_add(discarded, Ordering::SeqCst);
                    output.fill_with(|| T::from_sample(0.0));
                    return;
                }
                if !shared.enabled.load(Ordering::SeqCst) {
                    output.fill_with(|| T::from_sample(0.0));
                    return;
                }
                if !shared.prebuffered.load(Ordering::SeqCst) {
                    let buffered = shared
                        .queued_samples
                        .load(Ordering::SeqCst)
                        .saturating_sub(shared.played_samples.load(Ordering::SeqCst));
                    if buffered < shared.prebuffer_samples.load(Ordering::SeqCst)
                        && !shared.decode_done.load(Ordering::SeqCst)
                    {
                        output.fill_with(|| T::from_sample(0.0));
                        return;
                    }
                    shared.prebuffered.store(true, Ordering::SeqCst);
                }
                let mut played = 0_u64;
                // Media time only advances for samples that actually carried the timeline: real
                // audio, and the leading silence that stands in for timeline before the first
                // packet. An underrun renders silence the producer never sent, and a hold is delay
                // that has not happened yet — counting either would run the clock ahead of the
                // media and make every linked video frame look late for the rest of the session.
                let mut rendered = 0_u64;
                for sample in output {
                    if shared
                        .hold_silence_samples
                        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                            remaining.checked_sub(1)
                        })
                        .is_ok()
                    {
                        *sample = T::from_sample(0.0);
                        continue;
                    }
                    let emit_silence = shared
                        .gap_silence_samples
                        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                            remaining.checked_sub(1)
                        })
                        .is_ok()
                        || shared
                            .leading_silence_samples
                            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                                remaining.checked_sub(1)
                            })
                            .is_ok();
                    if emit_silence {
                        *sample = T::from_sample(0.0);
                        rendered += 1;
                    } else if let Some(value) = consumer.try_pop() {
                        *sample = T::from_sample(value);
                        played += 1;
                        rendered += 1;
                    } else {
                        *sample = T::from_sample(0.0);
                    }
                }
                shared.played_samples.fetch_add(played, Ordering::SeqCst);
                shared.rendered_samples.fetch_add(rendered, Ordering::SeqCst);
            },
            move |error| error_shared.set_error(format!("audio output stream error: {error}")),
            None,
        )
        .map_err(|error| io::Error::other(format!("could not build audio output: {error}")))
}

pub struct AudioDecoder {
    /// The verified layout of the FFmpeg this decoder reads structures from.
    abi: &'static ffmpeg::Abi,
    context: *mut c_void,
    packet: *mut c_void,
    frame: *mut c_void,
    resampler: *mut c_void,
    input_rate: c_int,
    input_channels: c_int,
    output_rate: c_int,
    output_channels: c_int,
    pending_trim_start: usize,
    pending_trim_end: usize,
}

impl AudioDecoder {
    fn new(
        config: &AudioConfiguration,
        output_rate: u32,
        output_channels: u16,
    ) -> io::Result<Self> {
        // Verify the linked FFmpeg's structure layout before touching any of it.
        let abi = ffmpeg::abi()?;
        let name =
            CString::new(config.codec.as_str()).map_err(|_| invalid("audio codec has NUL"))?;
        let codec = unsafe { avcodec_find_decoder_by_name(name.as_ptr()) };
        if codec.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("FFmpeg decoder {:?} is unavailable", config.codec),
            ));
        }
        let mut context = unsafe { avcodec_alloc_context3(codec) };
        let mut parameters = match ffmpeg::allocate_parameters() {
            Ok(parameters) => parameters,
            Err(error) => {
                unsafe { avcodec_free_context(&mut context) };
                return Err(error);
            },
        };
        if context.is_null() {
            ffmpeg::free_parameters(&mut parameters);
            return Err(io::Error::other("FFmpeg could not allocate audio decoder state"));
        }
        let result = (|| {
            let extradata = if config.extradata.is_empty() {
                None
            } else {
                let size = config
                    .extradata
                    .len()
                    .checked_add(AV_INPUT_BUFFER_PADDING_SIZE)
                    .ok_or_else(|| invalid("audio extradata size overflows"))?;
                let extradata: *mut u8 = unsafe { av_mallocz(size) }.cast();
                if extradata.is_null() {
                    return Err(io::Error::other("FFmpeg could not allocate audio extradata"));
                }
                unsafe {
                    ptr::copy_nonoverlapping(
                        config.extradata.as_ptr(),
                        extradata,
                        config.extradata.len(),
                    )
                };
                let length = c_int::try_from(config.extradata.len())
                    .map_err(|_| invalid("audio extradata exceeds i32"))?;
                Some((extradata, length))
            };
            unsafe {
                abi.set_parameters(
                    parameters,
                    AVMEDIA_TYPE_AUDIO,
                    ffmpeg::codec_id(codec),
                    ParameterValues { extradata, ..Default::default() },
                );
            }
            check_ffmpeg("could not configure audio decoder", unsafe {
                avcodec_parameters_to_context(context, parameters)
            })?;
            check_ffmpeg("could not set audio packet time base", unsafe {
                av_opt_set_q(context, c"pkt_timebase".as_ptr(), PACKET_TIME_BASE, 0)
            })?;
            let ar = c"ar";
            check_ffmpeg("could not set audio sample rate", unsafe {
                av_opt_set_int(context, ar.as_ptr(), i64::from(config.sample_rate), 0)
            })?;
            let mut layout = AVChannelLayout::default();
            if config.channel_mask == 0 {
                unsafe { av_channel_layout_default(&mut layout, c_int::from(config.channels)) };
            } else {
                check_ffmpeg("could not set audio channel mask", unsafe {
                    av_channel_layout_from_mask(&mut layout, config.channel_mask)
                })?;
            }
            let layout_result =
                unsafe { av_opt_set_chlayout(context, c"ch_layout".as_ptr(), &layout, 0) };
            unsafe { av_channel_layout_uninit(&mut layout) };
            check_ffmpeg("could not set audio channel layout", layout_result)?;
            check_ffmpeg("could not open audio decoder", unsafe {
                avcodec_open2(context, codec, ptr::null_mut())
            })
        })();
        ffmpeg::free_parameters(&mut parameters);
        if let Err(error) = result {
            unsafe { avcodec_free_context(&mut context) };
            return Err(error);
        }
        let packet = unsafe { av_packet_alloc() };
        let frame = unsafe { av_frame_alloc() };
        if packet.is_null() || frame.is_null() {
            let mut packet = packet;
            let mut frame = frame;
            unsafe {
                av_packet_free(&mut packet);
                av_frame_free(&mut frame);
                avcodec_free_context(&mut context);
            }
            return Err(io::Error::other("FFmpeg could not allocate audio decode buffers"));
        }
        Ok(Self {
            abi,
            context,
            packet,
            frame,
            resampler: ptr::null_mut(),
            input_rate: config.sample_rate as c_int,
            input_channels: config.channels as c_int,
            output_rate: output_rate as c_int,
            output_channels: output_channels as c_int,
            pending_trim_start: 0,
            pending_trim_end: 0,
        })
    }

    pub fn push(&mut self, packet: ParsedAudioPacket<'_>) -> io::Result<Vec<f32>> {
        if packet.data.is_empty() {
            return Ok(Vec::new());
        }
        let size = c_int::try_from(packet.data.len())
            .map_err(|_| invalid("audio packet exceeds FFmpeg i32 size"))?;
        check_ffmpeg("could not allocate audio packet", unsafe {
            av_new_packet(self.packet, size)
        })?;
        let av_packet = unsafe { &mut *(self.packet as *mut AVPacket) };
        unsafe {
            ptr::copy_nonoverlapping(packet.data.as_ptr(), av_packet.data, packet.data.len())
        };
        av_packet.pts = packet.pts_us;
        av_packet.dts = packet.dts_us;
        av_packet.duration = i64::try_from(packet.duration_us).unwrap_or(i64::MAX);
        av_packet.time_base = PACKET_TIME_BASE;
        let result = unsafe { avcodec_send_packet(self.context, self.packet) };
        unsafe { av_packet_unref(self.packet) };
        check_ffmpeg("audio decoder rejected packet", result)?;
        self.pending_trim_start = self.pending_trim_start.saturating_add(converted_trim_samples(
            packet.trim_start_samples,
            self.input_rate,
            self.output_rate,
            self.output_channels,
        ));
        self.pending_trim_end = self.pending_trim_end.saturating_add(converted_trim_samples(
            packet.trim_end_samples,
            self.input_rate,
            self.output_rate,
            self.output_channels,
        ));
        let mut samples = self.receive(false)?;
        trim_pending_samples(
            &mut samples,
            &mut self.pending_trim_start,
            &mut self.pending_trim_end,
        );
        Ok(samples)
    }

    pub fn finish(&mut self) -> io::Result<Vec<f32>> {
        let result = unsafe { avcodec_send_packet(self.context, ptr::null()) };
        if result < 0 && result != AVERROR_EOF {
            return Err(ffmpeg_error("could not drain audio decoder", result));
        }
        let mut samples = self.receive(true)?;
        if !self.resampler.is_null() {
            loop {
                let mut output = vec![0.0_f32; 4096 * self.output_channels as usize];
                let mut planes = [output.as_mut_ptr().cast::<u8>()];
                let converted = unsafe {
                    swr_convert(self.resampler, planes.as_mut_ptr(), 4096, ptr::null(), 0)
                };
                check_ffmpeg("could not drain audio resampler", converted)?;
                if converted == 0 {
                    break;
                }
                output.truncate(converted as usize * self.output_channels as usize);
                samples.extend(output);
            }
        }
        trim_pending_samples(
            &mut samples,
            &mut self.pending_trim_start,
            &mut self.pending_trim_end,
        );
        Ok(samples)
    }

    fn receive(&mut self, _draining: bool) -> io::Result<Vec<f32>> {
        let mut samples = Vec::new();
        loop {
            let result = unsafe { avcodec_receive_frame(self.context, self.frame) };
            if result == -libc::EAGAIN || result == AVERROR_EOF {
                break;
            }
            check_ffmpeg("could not receive decoded audio", result)?;
            let frame = unsafe { self.abi.frame(self.frame) };
            if frame.nb_samples <= 0 {
                unsafe { av_frame_unref(self.frame) };
                continue;
            }
            if self.resampler.is_null() {
                self.init_resampler(frame.format)?;
            }
            let maximum = (i64::from(frame.nb_samples) * i64::from(self.output_rate)
                / i64::from(self.input_rate)
                + 256)
                .clamp(1, i64::from(c_int::MAX)) as c_int;
            let mut output = vec![0.0_f32; maximum as usize * self.output_channels as usize];
            let mut output_planes = [output.as_mut_ptr().cast::<u8>()];
            let input = if frame.extended_data.is_null() {
                frame.data.as_ptr() as *const *const u8
            } else {
                frame.extended_data as *const *const u8
            };
            let converted = unsafe {
                swr_convert(
                    self.resampler,
                    output_planes.as_mut_ptr(),
                    maximum,
                    input,
                    frame.nb_samples,
                )
            };
            check_ffmpeg("could not resample audio", converted)?;
            output.truncate(converted as usize * self.output_channels as usize);
            samples.extend(output);
            unsafe { av_frame_unref(self.frame) };
        }
        Ok(samples)
    }

    fn init_resampler(&mut self, input_format: c_int) -> io::Result<()> {
        let mut input = AVChannelLayout::default();
        let mut output = AVChannelLayout::default();
        unsafe {
            av_channel_layout_default(&mut input, self.input_channels);
            av_channel_layout_default(&mut output, self.output_channels);
        }
        let result = unsafe {
            swr_alloc_set_opts2(
                &mut self.resampler,
                &output,
                AV_SAMPLE_FMT_FLT,
                self.output_rate,
                &input,
                input_format,
                self.input_rate,
                0,
                ptr::null_mut(),
            )
        };
        unsafe {
            av_channel_layout_uninit(&mut input);
            av_channel_layout_uninit(&mut output);
        }
        check_ffmpeg("could not allocate audio resampler", result)?;
        check_ffmpeg("could not initialize audio resampler", unsafe { swr_init(self.resampler) })
    }

    #[cfg(test)]
    fn packet_time_base(&self) -> io::Result<AVRational> {
        let mut time_base = AVRational { num: 0, den: 0 };
        check_ffmpeg("could not read audio packet time base", unsafe {
            av_opt_get_q(self.context, c"pkt_timebase".as_ptr(), 0, &mut time_base)
        })?;
        Ok(time_base)
    }
}

impl Drop for AudioDecoder {
    fn drop(&mut self) {
        unsafe {
            swr_free(&mut self.resampler);
            av_frame_free(&mut self.frame);
            av_packet_free(&mut self.packet);
            avcodec_free_context(&mut self.context);
        }
    }
}

#[cfg(test)]
fn trim_samples(
    samples: &mut Vec<f32>,
    trim_start_samples: u32,
    trim_end_samples: u32,
    input_rate: c_int,
    output_rate: c_int,
    output_channels: c_int,
) {
    if input_rate <= 0 || output_rate <= 0 || output_channels <= 0 || samples.is_empty() {
        return;
    }
    let mut start =
        converted_trim_samples(trim_start_samples, input_rate, output_rate, output_channels);
    let mut end =
        converted_trim_samples(trim_end_samples, input_rate, output_rate, output_channels);
    trim_pending_samples(samples, &mut start, &mut end);
}

fn leading_silence_sample_count(
    timeline_origin_us: i64,
    audio_origin_us: i64,
    sample_rate: u32,
    channels: u16,
) -> u64 {
    (audio_origin_us.saturating_sub(timeline_origin_us).max(0) as u64)
        .saturating_mul(u64::from(sample_rate))
        / 1_000_000
        * u64::from(channels)
}

fn rendered_pts_us(origin_us: i64, samples: u64, sample_rate: u32, channels: u16) -> i64 {
    if sample_rate == 0 || channels == 0 {
        return origin_us;
    }
    let frames = samples / u64::from(channels);
    let elapsed_us = frames.saturating_mul(1_000_000) / u64::from(sample_rate);
    origin_us.saturating_add(i64::try_from(elapsed_us).unwrap_or(i64::MAX))
}

fn converted_trim_samples(
    input_samples: u32,
    input_rate: c_int,
    output_rate: c_int,
    output_channels: c_int,
) -> usize {
    if input_rate <= 0 || output_rate <= 0 || output_channels <= 0 {
        return 0;
    }
    u64::from(input_samples)
        .saturating_mul(output_rate as u64)
        .div_ceil(input_rate as u64)
        .saturating_mul(output_channels as u64)
        .min(usize::MAX as u64) as usize
}

fn trim_pending_samples(samples: &mut Vec<f32>, start: &mut usize, end: &mut usize) {
    let remove_start = (*start).min(samples.len());
    if remove_start > 0 {
        samples.drain(..remove_start);
        *start -= remove_start;
    }
    let remove_end = (*end).min(samples.len());
    samples.truncate(samples.len().saturating_sub(remove_end));
    *end -= remove_end;
}

fn check_ffmpeg(context: &str, result: c_int) -> io::Result<()> {
    if result < 0 { Err(ffmpeg_error(context, result)) } else { Ok(()) }
}

fn ffmpeg_error(context: &str, code: c_int) -> io::Error {
    let mut buffer = [0_i8; 256];
    let description = if unsafe { av_strerror(code, buffer.as_mut_ptr(), buffer.len()) } == 0 {
        unsafe { CStr::from_ptr(buffer.as_ptr()) }.to_string_lossy().into_owned()
    } else {
        format!("FFmpeg error {code}")
    };
    io::Error::other(format!("{context}: {description}"))
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[link(name = "avcodec")]
unsafe extern "C" {}
#[link(name = "avutil")]
unsafe extern "C" {}
#[link(name = "swresample")]
unsafe extern "C" {}

unsafe extern "C" {
    fn avcodec_find_decoder_by_name(name: *const c_char) -> *const c_void;
    fn avcodec_alloc_context3(codec: *const c_void) -> *mut c_void;
    fn avcodec_free_context(context: *mut *mut c_void);
    fn avcodec_parameters_to_context(context: *mut c_void, parameters: *const c_void) -> c_int;
    fn avcodec_open2(
        context: *mut c_void,
        codec: *const c_void,
        options: *mut *mut c_void,
    ) -> c_int;
    fn avcodec_send_packet(context: *mut c_void, packet: *const c_void) -> c_int;
    fn avcodec_receive_frame(context: *mut c_void, frame: *mut c_void) -> c_int;
    fn av_packet_alloc() -> *mut c_void;
    fn av_packet_free(packet: *mut *mut c_void);
    fn av_packet_unref(packet: *mut c_void);
    fn av_new_packet(packet: *mut c_void, size: c_int) -> c_int;
    fn av_frame_alloc() -> *mut c_void;
    fn av_frame_free(frame: *mut *mut c_void);
    fn av_frame_unref(frame: *mut c_void);
    fn av_mallocz(size: usize) -> *mut c_void;
    fn av_opt_set_q(
        object: *mut c_void,
        name: *const c_char,
        value: AVRational,
        flags: c_int,
    ) -> c_int;
    #[cfg(test)]
    fn av_opt_get_q(
        object: *mut c_void,
        name: *const c_char,
        flags: c_int,
        output: *mut AVRational,
    ) -> c_int;
    fn av_opt_set_int(object: *mut c_void, name: *const c_char, value: i64, flags: c_int) -> c_int;
    fn av_opt_set_chlayout(
        object: *mut c_void,
        name: *const c_char,
        layout: *const AVChannelLayout,
        flags: c_int,
    ) -> c_int;
    fn av_channel_layout_default(layout: *mut AVChannelLayout, channels: c_int);
    fn av_channel_layout_from_mask(layout: *mut AVChannelLayout, mask: u64) -> c_int;
    fn av_channel_layout_uninit(layout: *mut AVChannelLayout);
    fn swr_alloc_set_opts2(
        context: *mut *mut c_void,
        output_layout: *const AVChannelLayout,
        output_format: c_int,
        output_rate: c_int,
        input_layout: *const AVChannelLayout,
        input_format: c_int,
        input_rate: c_int,
        log_offset: c_int,
        log_context: *mut c_void,
    ) -> c_int;
    fn swr_init(context: *mut c_void) -> c_int;
    fn swr_convert(
        context: *mut c_void,
        output: *mut *mut u8,
        output_count: c_int,
        input: *const *const u8,
        input_count: c_int,
    ) -> c_int;
    fn swr_free(context: *mut *mut c_void);
    fn av_strerror(error: c_int, buffer: *mut c_char, buffer_size: usize) -> c_int;
}

#[cfg(test)]
mod tests {
    use super::*;
    use vivid_protocol::media::ParsedAudioPacket;
    use vivid_protocol::track::AudioConfiguration;

    #[test]
    fn video_gate_stall_requires_enabled_and_absent_clock_progress() {
        let output = AudioOutput::test_output();
        assert!(!output.video_gate_stalled());

        output.configure_play(0, 100_000);
        output.start();
        assert!(!output.video_gate_stalled());
        output.force_video_gate_stall_for_test();
        assert!(output.video_gate_stalled());
        assert_eq!(output.shared.error().as_deref(), Some("linked audio output clock stalled"));

        output.pause();
        assert!(!output.video_gate_stalled());
        output.flush();
        assert!(!output.video_gate_stalled());
        assert!(
            output
                .shared
                .play_configured_at
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_none()
        );
    }

    #[test]
    fn video_gate_observes_audio_clock_progress_before_declaring_a_stall() {
        let output = AudioOutput::test_output();
        output.configure_play(0, 100_000);
        output.start();
        output.force_video_gate_stall_for_test();
        output.shared.rendered_samples.store(256, Ordering::SeqCst);
        assert!(!output.video_gate_stalled());
        assert!(output.shared.error().is_none());
    }

    #[test]
    fn trim_is_rescaled_and_channel_aligned() {
        let mut samples: Vec<f32> = (0..20).map(|value| value as f32).collect();
        trim_samples(&mut samples, 2, 1, 24_000, 48_000, 2);
        assert_eq!(samples, (8..16).map(|value| value as f32).collect::<Vec<_>>());
    }

    #[test]
    fn delayed_decoder_output_preserves_pending_trim() {
        let mut start = 4;
        let mut end = 2;
        let mut empty = Vec::new();
        trim_pending_samples(&mut empty, &mut start, &mut end);
        assert_eq!((start, end), (4, 2));

        let mut samples: Vec<f32> = (0..12).map(|value| value as f32).collect();
        trim_pending_samples(&mut samples, &mut start, &mut end);
        assert_eq!(samples, (4..10).map(|value| value as f32).collect::<Vec<_>>());
        assert_eq!((start, end), (0, 0));
    }

    #[test]
    fn rendered_audio_frames_are_the_media_clock() {
        assert_eq!(rendered_pts_us(20_000, 48_000 * 2, 48_000, 2), 1_020_000);
        assert_eq!(leading_silence_sample_count(0, 100_000, 48_000, 2), 9_600);
        assert_eq!(leading_silence_sample_count(100_000, 0, 48_000, 2), 0);
    }

    #[test]
    fn live_video_discard_threshold_tracks_rendered_audio() {
        let output = AudioOutput::test_output();
        output.observe_audio_pts(1_000_000);
        output.start();
        output.shared.prebuffered.store(true, Ordering::SeqCst);
        output.shared.rendered_samples.store(48_000 * 2, Ordering::SeqCst);

        assert_eq!(output.discard_video_before(100_000), Some(1_900_000));
    }

    #[test]
    fn a_live_delay_holds_audio_back_without_moving_the_media_clock() {
        // Regression: audio played on arrival runs ahead of the picture it belongs with by the
        // transport's video delay. The hold is what puts them back together, and it must not be
        // mistaken for elapsed media time — a delay that advanced the clock would leave the video
        // exactly as late as it was before, and be discarded for it.
        let output = AudioOutput::test_output();
        output.observe_audio_pts(1_000_000);
        output.start();
        output.shared.prebuffered.store(true, Ordering::SeqCst);
        let before = output.rendered_pts();

        assert_eq!(output.request_live_delay(300_000), 300_000);
        assert_eq!(output.live_delay_us(), 300_000);
        assert_eq!(output.shared.hold_silence_samples.load(Ordering::SeqCst), 48_000 * 2 * 3 / 10);
        assert_eq!(output.rendered_pts(), before, "a hold is not elapsed media time");

        // Jitter-sized changes are ignored, and the hold is bounded.
        assert_eq!(output.request_live_delay(310_000), 300_000);
        assert_eq!(output.request_live_delay(u64::MAX), MAXIMUM_LIVE_DELAY_US);
    }

    #[test]
    fn shrinking_a_live_delay_discards_the_samples_it_no_longer_holds() {
        let output = AudioOutput::test_output();
        output.request_live_delay(500_000);

        assert_eq!(output.request_live_delay(200_000), 200_000);
        assert_eq!(output.live_delay_us(), 200_000);
        assert_eq!(output.shared.discard_samples.load(Ordering::SeqCst), 48_000 * 2 * 3 / 10);
    }

    #[test]
    fn bridging_a_gap_advances_the_clock_and_keeps_every_buffered_sample() {
        // The producer dropped 120 ms. The timeline is intact, so the clock must move over the
        // hole while the audio already buffered behind it survives; restarting the timeline for
        // this is audible and used to happen for any step over 2 ms.
        let output = AudioOutput::test_output();
        output.observe_audio_pts(1_000_000);
        output.start();
        output.shared.prebuffered.store(true, Ordering::SeqCst);
        output.shared.queued_samples.store(9_600, Ordering::SeqCst);

        output.bridge_live_gap(120_000);
        // The packet that follows the gap is observed like any other. Leading silence is
        // recomputed and *stored* on every observation, so a bridge kept there would be erased
        // before a single sample of it reached the device.
        output.observe_audio_pts(1_200_000);

        assert_eq!(output.shared.gap_silence_samples.load(Ordering::SeqCst), 48_000 * 2 * 12 / 100);
        assert_eq!(
            output.shared.leading_silence_samples.load(Ordering::SeqCst),
            0,
            "the observation really does clear leading silence, which is why the bridge is separate"
        );
        assert_eq!(output.shared.discard_samples.load(Ordering::SeqCst), 0);
        assert_eq!(output.shared.queued_samples.load(Ordering::SeqCst), 9_600);
        assert_eq!(
            output.shared.timeline_origin_us.load(Ordering::SeqCst),
            1_000_000,
            "a gap does not restart the timeline"
        );
    }

    #[test]
    fn live_rebase_discards_old_samples_and_restarts_the_clock() {
        let output = AudioOutput::test_output();
        output.observe_audio_pts(1_000_000);
        output.start();
        output.shared.queued_samples.store(960, Ordering::SeqCst);
        output.shared.played_samples.store(480, Ordering::SeqCst);

        output.rebase_live(2_000_000);

        assert!(output.shared.enabled.load(Ordering::SeqCst));
        assert_eq!(output.shared.timeline_origin_us.load(Ordering::SeqCst), 2_000_000);
        assert_eq!(output.shared.discard_samples.load(Ordering::SeqCst), 480);
        assert!(!output.shared.prebuffered.load(Ordering::SeqCst));
    }

    #[test]
    fn ffmpeg_decodes_pcm_access_units_without_an_output_device() {
        let config = AudioConfiguration {
            codec: "pcm_s16le".into(),
            packetization: "pcm-packet-v1".into(),
            extradata: Vec::new(),
            sample_rate: 48_000,
            channels: 2,
            channel_mask: 3,
            maximum_access_unit_bytes: 4_096,
            codec_string: None,
        };
        let encoded = vec![0_u8; 480 * 2 * 2];
        assert!(supports(&config));
        let mut decoder = AudioDecoder::new(&config, 48_000, 2).unwrap();
        assert_eq!(decoder.packet_time_base().unwrap(), PACKET_TIME_BASE);
        let mut samples = decoder
            .push(ParsedAudioPacket {
                epoch: 1,
                packet_id: 1,
                pts_us: 0,
                dts_us: 0,
                duration_us: 10_000,
                trim_start_samples: 0,
                trim_end_samples: 0,
                data: &encoded,
            })
            .unwrap();
        samples.extend(decoder.finish().unwrap());
        assert_eq!(samples.len(), 480 * 2);
        assert!(samples.iter().all(|sample| *sample == 0.0));
    }
}
