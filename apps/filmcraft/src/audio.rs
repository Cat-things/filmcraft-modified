//! cpal audio output: the playback master clock. Settings ▸ Audio Hardware picks the host
//! ("Device Class"), the output device, the I/O buffer size and the sample rate.
//!
//! A stream the device drops while playing (an ALSA xrun the backend does not recover from, which
//! cpal reports as `poll() returned POLLERR`; an unplugged device) is restarted by
//! [`AudioOut::recover`], keeping the played-frame clock and the play-ahead mixer, so sound
//! resumes where it stopped instead of playback going silent (#840).
//!
//! The playback clock is what the listener hears ([`Audible`]): frames handed to the device minus
//! the ones still queued in it. Without that, the picture ran ahead of the sound by the device's
//! output latency, which is large through PipeWire / PulseAudio on Linux.
//!
//! On ALSA the I/O buffer size setting is the period (frames mixed per callback) and the ring holds
//! four of them, at least [`ALSA_MIN_RING`]: cpal takes a fixed size as the whole ring, so the
//! default 512 left ~10 ms of slack and crackled or failed with xruns on Linux desktops.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use std::sync::{Arc, Mutex, PoisonError, TryLockError};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use filmcraft_engine::settings::AudioHardwarePrefs;
use filmcraft_ui_egui::{AudioDevices, AudioOut};

/// The playback mixer callback handed to [`AudioOut::start`].
type Fill = Box<dyn FnMut(&mut [f32], usize) + Send>;

pub struct CpalOut {
    stream: Option<cpal::Stream>,
    /// The running playback's mixer, shared with the stream so a restarted stream continues it.
    fill: Option<Arc<Mutex<Fill>>>,
    /// Frames handed to the device since `start`.
    played: Arc<AtomicU64>,
    /// Frames heard since `start` (the playback clock).
    audible: Arc<Audible>,
    health: Arc<Health>,
    watchdog: Watchdog,
    /// Time base for the watchdog.
    epoch: Instant,
    rate: u32,
    channels: u16,
    hw: AudioHardwarePrefs,
    /// Sequence sample rate for "Attempt to force hardware to document sample rate".
    document_rate: Option<u32>,
    /// Status-bar note: the host's default device could not be opened and a fallback device is
    /// in use, or no output device was found (playing without sound).
    note: Option<Note>,
    /// The fallback picked when the default device could not be opened, as (host, device) names,
    /// so later lookups (`devices`, `configure`, `start`) reuse it instead of probing every PCM.
    fallback: Mutex<Option<(String, String)>>,
    /// The device chosen by name in the settings, as (host, name, device): finding it means
    /// listing every device, which on ALSA opens every PCM and took long enough to delay the
    /// start of each playback.
    named: Mutex<Option<(String, String, cpal::Device)>>,
}

/// The ALSA ring never holds less than this (≈43 ms at 48 kHz): PipeWire's and PulseAudio's ALSA
/// plugins run their own graph on top, and a GUI app's mixer thread is not real-time.
const ALSA_MIN_RING: u32 = 2048;
const ALSA_MAX_RING: u32 = 16_384;

/// The fixed buffer size to ask cpal for: the I/O buffer size setting is the number of frames
/// mixed per callback (as in the setting's other hosts), and cpal's ALSA backend takes a fixed
/// size as the whole ring split into four periods.
fn device_buffer_frames(host: &str, setting: u32) -> u32 {
    if host.eq_ignore_ascii_case("alsa") { setting.saturating_mul(4).clamp(ALSA_MIN_RING, ALSA_MAX_RING) } else { setting.max(1) }
}

/// A device's reported output latency is trusted up to this (a broken driver's value is not).
const MAX_LATENCY: Duration = Duration::from_secs(2);

/// The audible playback position. The audio thread records, per callback, how many frames had
/// been heard then (frames handed to the device before the buffer, minus the device's latency:
/// cpal's gap between the callback and playback timestamps); the UI thread interpolates from there
/// on the wall clock, never past the frames handed over and never backwards.
#[derive(Default)]
struct Audible {
    /// (frames heard, when the callback ran, as time since the output's epoch).
    last: Mutex<Option<(u64, Duration)>>,
    /// The largest value reported (the clock never runs backwards).
    shown: AtomicU64,
}

impl Audible {
    /// Audio thread: a buffer was asked for after `before` frames had been handed over; its first
    /// frame plays `latency` later. Skipped when the UI thread holds the lock (never blocks).
    fn on_callback(&self, before: u64, latency: Duration, rate: u32, at: Duration) {
        let queued = (latency.min(MAX_LATENCY).as_secs_f64() * f64::from(rate)) as u64;
        if let Ok(mut last) = self.last.try_lock() {
            *last = Some((before.saturating_sub(queued), at));
        }
    }

    /// UI thread: frames heard by `now`, of the `delivered` frames handed over.
    fn frames(&self, delivered: u64, rate: u32, now: Duration) -> u64 {
        let last = *self.last.lock().unwrap_or_else(PoisonError::into_inner);
        let heard = match last {
            Some((frames, at)) => frames.saturating_add((now.saturating_sub(at).as_secs_f64() * f64::from(rate)) as u64),
            None => 0,
        }
        .min(delivered);
        self.shown.fetch_max(heard, Ordering::SeqCst).max(heard)
    }
}

/// The output latency cpal reports for one callback.
fn callback_latency(info: &cpal::OutputCallbackInfo) -> Duration {
    let ts = info.timestamp();
    ts.playback.duration_since(&ts.callback).unwrap_or(Duration::ZERO)
}

/// A hardware note for the status bar, translated when it is shown.
#[derive(Clone, Debug, PartialEq)]
enum Note {
    /// The default output device cannot be opened; this named device is used instead.
    Fallback(String),
    /// No output device at all: playback runs without sound.
    NoDevice,
}

impl Note {
    fn text(&self) -> String {
        use filmcraft_ui_egui::i18n::{fmt, t};
        match self {
            Note::Fallback(device) => fmt(t("Audio hardware: the default output device cannot be opened; using '{device}'"), &[("device", device)]),
            Note::NoDevice => t("No audio output device found: playing without sound (check Settings ▸ Audio Hardware)").to_string(),
        }
    }
}

/// The host named in the settings (empty or unknown = the default host).
fn host(name: &str) -> cpal::Host {
    if !name.is_empty()
        && let Some(id) = cpal::available_hosts().into_iter().find(|h| h.name() == name)
        && let Ok(h) = cpal::host_from_id(id)
    {
        return h;
    }
    cpal::default_host()
}

/// The output device the settings ask for, with a status-bar note when a fallback is in use.
struct DeviceChoice {
    device: cpal::Device,
    /// The host's default device could not be opened: this device is the fallback.
    fallback: Option<Note>,
}

/// Fallback order when the default device cannot be opened: the sound-server bridges first
/// (`pipewire`, `pulse`: they share the card with every other app), then other named PCMs, and
/// raw `hw:`/`plughw:` devices last (opening one takes the card exclusively and silences the
/// rest of the desktop).
pub(crate) fn fallback_rank(name: &str) -> u8 {
    let n = name.to_ascii_lowercase();
    if n == "pipewire" || n.starts_with("pipewire:") {
        0
    } else if n == "pulse" || n.starts_with("pulse:") {
        1
    } else if n.starts_with("hw:") || n.starts_with("plughw:") {
        3
    } else {
        2
    }
}

/// The output device named in the settings (empty or missing = the host's default). When the
/// default cannot be opened — a broken ALSA `default` (missing `99-pipewire-default.conf`,
/// #23/#106) — a fallback that can, so the app is not silently mute. The fallback list is probed
/// only after the default fails, in [`fallback_rank`] order, and the pick is cached in `cache`.
fn output_device(
    h: &cpal::Host,
    name: &str,
    cache: &Mutex<Option<(String, String)>>,
    named: &Mutex<Option<(String, String, cpal::Device)>>,
) -> Option<DeviceChoice> {
    if !name.is_empty() {
        let host_name = h.id().name();
        let mut named = named.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((cached_host, cached, d)) = named.as_ref()
            && cached_host == host_name
            && cached == name
        {
            return Some(DeviceChoice { device: d.clone(), fallback: None });
        }
        if let Ok(mut devs) = h.output_devices()
            && let Some(d) = devs.find(|d| d.name().is_ok_and(|n| n == name))
        {
            *named = Some((host_name.to_string(), name.to_string(), d.clone()));
            return Some(DeviceChoice { device: d, fallback: None });
        }
    }
    if let Some(d) = h.default_output_device()
        && d.default_output_config().is_ok()
    {
        return Some(DeviceChoice { device: d, fallback: None });
    }
    let host_name = h.id().name().to_string();
    let mut cache = cache.lock().unwrap_or_else(PoisonError::into_inner);
    let mut devs: Vec<(String, cpal::Device)> = h.output_devices().ok()?.filter_map(|d| d.name().ok().map(|n| (n, d))).collect();
    if let Some((cached_host, cached)) = cache.as_ref()
        && *cached_host == host_name
        && let Some(i) = devs.iter().position(|(n, _)| n == cached)
    {
        let (found, d) = devs.swap_remove(i);
        return Some(DeviceChoice { device: d, fallback: Some(Note::Fallback(found)) });
    }
    devs.sort_by_key(|(n, _)| fallback_rank(n));
    let (found, d) = devs.into_iter().find(|(_, d)| d.default_output_config().is_ok())?;
    log::warn!("the default audio output device cannot be opened; falling back to '{found}'");
    *cache = Some((host_name, found.clone()));
    Some(DeviceChoice { device: d, fallback: Some(Note::Fallback(found)) })
}

impl CpalOut {
    pub fn new() -> Self {
        let host = cpal::default_host();
        let fallback = Mutex::new(None);
        let named = Mutex::new(None);
        let choice = output_device(&host, "", &fallback, &named);
        let cfg = choice.as_ref().and_then(|c| c.device.default_output_config().ok());
        let note = match choice {
            Some(c) => c.fallback,
            None => {
                log::warn!("no audio output device found");
                Some(Note::NoDevice)
            }
        };
        Self {
            stream: None,
            fill: None,
            played: Arc::new(AtomicU64::new(0)),
            audible: Arc::new(Audible::default()),
            health: Arc::new(Health::default()),
            watchdog: Watchdog::default(),
            epoch: Instant::now(),
            rate: cfg.as_ref().map_or(48_000, |c| c.sample_rate().0),
            channels: cfg.as_ref().map_or(2, |c| c.channels()),
            hw: AudioHardwarePrefs::default(),
            document_rate: None,
            note,
            fallback,
            named,
        }
    }

    /// The stream configuration the settings ask for, falling back to the device default.
    fn config(&self, dev: &cpal::Device) -> Result<cpal::SupportedStreamConfig, String> {
        let default = dev.default_output_config().map_err(|e| e.to_string())?;
        let want = if self.hw.force_document_rate { self.document_rate.unwrap_or(self.hw.sample_rate) } else { self.hw.sample_rate };
        let exact = dev.supported_output_configs().ok().and_then(|it| {
            it.filter(|c| c.channels() > 0 && (c.min_sample_rate().0..=c.max_sample_rate().0).contains(&want))
                .max_by_key(|c| {
                    (c.channels() == default.channels(), c.sample_format() == cpal::SampleFormat::F32, c.sample_format() == default.sample_format())
                })
                .and_then(|c| c.try_with_sample_rate(cpal::SampleRate(want)))
        });
        Ok(exact.unwrap_or(default))
    }

    /// One device callback, as both stream kinds run it: mix into `buf`, advance the frames
    /// handed over, and note what has been heard.
    fn callback(&self, rate: u32, fill: Arc<Mutex<Fill>>) -> impl FnMut(&mut [f32], usize, &cpal::OutputCallbackInfo) + Send + 'static {
        let (played, audible, health, epoch) = (self.played.clone(), self.audible.clone(), self.health.clone(), self.epoch);
        move |buf, channels, info| {
            let before = played.load(Ordering::SeqCst);
            render(&fill, buf, channels, &played, &health);
            audible.on_callback(before, callback_latency(info), rate, epoch.elapsed());
        }
    }

    fn converted_stream<T>(
        &self,
        dev: &cpal::Device,
        config: &cpal::StreamConfig,
        mut callback: impl FnMut(&mut [f32], usize, &cpal::OutputCallbackInfo) + Send + 'static,
    ) -> Result<cpal::Stream, cpal::BuildStreamError>
    where
        T: cpal::SizedSample + cpal::FromSample<f32>,
    {
        let channels = usize::from(config.channels);
        let health = self.health.clone();
        let error_health = health.clone();
        let mut scratch = Vec::new();
        dev.build_output_stream(
            config,
            move |buf: &mut [T], info| {
                // Reuse the conversion buffer. A pathological driver buffer must not panic the audio thread.
                if buf.len() > 1_048_576 || scratch.try_reserve(buf.len().saturating_sub(scratch.len())).is_err() {
                    buf.fill(T::EQUILIBRIUM);
                    if !health.failed.swap(true, Ordering::SeqCst) {
                        eprintln!("filmcraft: unable to allocate the audio conversion buffer");
                    }
                    return;
                }
                scratch.resize(buf.len(), 0.0);
                callback(&mut scratch, channels, info);
                convert_samples(&scratch, buf);
            },
            move |e| stream_error(&error_health, e),
            None,
        )
    }

    /// Build (not start) an output stream playing `fill` in `format`.
    fn build(&self, dev: &cpal::Device, config: &cpal::StreamConfig, format: cpal::SampleFormat, fill: Arc<Mutex<Fill>>) -> Result<cpal::Stream, String> {
        let channels = usize::from(config.channels);
        let cb = self.callback(config.sample_rate.0, fill);
        match format {
            cpal::SampleFormat::F32 => {
                let mut cb = cb;
                let error_health = self.health.clone();
                dev.build_output_stream(config, move |buf: &mut [f32], info| cb(buf, channels, info), move |e| stream_error(&error_health, e), None)
            }
            cpal::SampleFormat::I8 => self.converted_stream::<i8>(dev, config, cb),
            cpal::SampleFormat::I16 => self.converted_stream::<i16>(dev, config, cb),
            cpal::SampleFormat::I24 => self.converted_stream::<cpal::I24>(dev, config, cb),
            cpal::SampleFormat::I32 => self.converted_stream::<i32>(dev, config, cb),
            cpal::SampleFormat::I64 => self.converted_stream::<i64>(dev, config, cb),
            cpal::SampleFormat::U8 => self.converted_stream::<u8>(dev, config, cb),
            cpal::SampleFormat::U16 => self.converted_stream::<u16>(dev, config, cb),
            cpal::SampleFormat::U32 => self.converted_stream::<u32>(dev, config, cb),
            cpal::SampleFormat::U64 => self.converted_stream::<u64>(dev, config, cb),
            cpal::SampleFormat::F64 => self.converted_stream::<f64>(dev, config, cb),
            other => return Err(format!("unsupported sample format {other:?}")),
        }
        .map_err(|e| e.to_string())
    }

    /// Open the configured output device and play `fill` on it. `expect` is the (sample rate,
    /// channels) a restarted stream must keep: the mixer was set up for them.
    fn open(&mut self, fill: Arc<Mutex<Fill>>, expect: Option<(u32, u16)>) -> Result<(), String> {
        let host = host(&self.hw.device_class);
        let DeviceChoice { device: dev, fallback } = output_device(&host, &self.hw.default_output, &self.fallback, &self.named).ok_or("no output device")?;
        self.note = fallback;
        let cfg = self.config(&dev)?;
        let channels = cfg.channels() as usize;
        if channels == 0 || cfg.sample_rate().0 == 0 {
            return Err("the output device returned an invalid channel count or sample rate".into());
        }
        if let Some((rate, ch)) = expect
            && (rate, ch) != (cfg.sample_rate().0, cfg.channels())
        {
            return Err(format!("the output device now runs at {} Hz with {} channels", cfg.sample_rate().0, cfg.channels()));
        }
        let mut config: cpal::StreamConfig = cfg.clone().into();
        let frames = device_buffer_frames(host.id().name(), self.hw.buffer_size);
        if let cpal::SupportedBufferSize::Range { min, max } = cfg.buffer_size()
            && (*min..=*max).contains(&frames)
        {
            config.buffer_size = cpal::BufferSize::Fixed(frames);
        }
        self.rate = cfg.sample_rate().0;
        self.channels = cfg.channels();
        let stream = match self.build(&dev, &config, cfg.sample_format(), fill.clone()) {
            Ok(stream) => stream,
            // A buffer size the device refuses (cpal sets an ALSA ring to exactly that size):
            // let the backend choose rather than playing without sound.
            Err(e) if matches!(config.buffer_size, cpal::BufferSize::Fixed(_)) => {
                log::warn!("the audio output refused a {frames}-frame buffer ({e}); using the device's default");
                config.buffer_size = cpal::BufferSize::Default;
                self.build(&dev, &config, cfg.sample_format(), fill)?
            }
            Err(e) => return Err(e),
        };
        stream.play().map_err(|e| e.to_string())?;
        self.stream = Some(stream);
        Ok(())
    }

    /// Replace a dropped stream with a new one on the same mixer, clock and format.
    fn restart(&mut self) -> Result<(), String> {
        let fill = self.fill.clone().ok_or("nothing is playing")?;
        let expect = (self.rate, self.channels);
        // The old stream (and its audio thread) goes first: only one stream ever runs the mixer.
        self.stream = None;
        self.health.clear_errors();
        // a device that went away may come back as a new device under the same name
        *self.named.lock().unwrap_or_else(PoisonError::into_inner) = None;
        self.open(fill, Some(expect))
    }
}

/// What the audio thread reports to the UI thread.
#[derive(Default)]
struct Health {
    /// The device went away (unplugged): restart the stream at once.
    lost: AtomicBool,
    /// A backend error since the last check (cpal keeps the stream alive after these): restart
    /// only if the stream also stopped consuming samples.
    errored: AtomicBool,
    /// Unrecoverable: the mixer panicked, a buffer could not be allocated, or restarts ran out.
    /// [`AudioOut::played_frames`] reports `None` and playback goes on without sound.
    failed: AtomicBool,
    /// The first error of a stream has been printed.
    reported: AtomicBool,
}

impl Health {
    fn clear_errors(&self) {
        self.lost.store(false, Ordering::SeqCst);
        self.errored.store(false, Ordering::SeqCst);
        self.reported.store(false, Ordering::SeqCst);
    }
}

/// How long a stream that reported an error may go without consuming samples before it is
/// restarted. Well under the UI's `AUDIO_STALL_S`, which gives up on the device clock.
const STALL_RESTART: Duration = Duration::from_millis(300);
/// At most this many restarts within [`RESTART_WINDOW`]; a device that keeps failing gives up.
const MAX_RESTARTS: usize = 3;
const RESTART_WINDOW: Duration = Duration::from_secs(10);

/// What [`Watchdog::check`] decides about the running stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verdict {
    Healthy,
    Restart,
    GiveUp,
}

/// Decides when a stream that reported errors is restarted (UI thread, every frame).
#[derive(Default)]
struct Watchdog {
    /// Played-frame count last seen, and when it last changed.
    seen: (u64, Duration),
    /// An error was reported and the stream has not consumed samples since.
    pending_error: bool,
    /// When recent restarts happened (at most [`MAX_RESTARTS`] kept).
    restarts: Vec<Duration>,
}

impl Watchdog {
    fn reset(&mut self, now: Duration) {
        *self = Self { seen: (0, now), ..Self::default() };
    }

    fn check(&mut self, now: Duration, played: u64, errored: bool, lost: bool) -> Verdict {
        if played != self.seen.0 {
            self.seen = (played, now);
            self.pending_error = false;
        }
        // An error that arrives with the stream still consuming samples is only a hiccup.
        self.pending_error |= errored;
        let stalled = now.saturating_sub(self.seen.1) >= STALL_RESTART;
        if !lost && !(self.pending_error && stalled) {
            return Verdict::Healthy;
        }
        self.restarts.retain(|t| now.saturating_sub(*t) < RESTART_WINDOW);
        if self.restarts.len() >= MAX_RESTARTS {
            return Verdict::GiveUp;
        }
        self.restarts.push(now);
        self.seen = (played, now);
        self.pending_error = false;
        Verdict::Restart
    }
}

/// One device callback: run the shared mixer into `buf` and advance the clock. Never panics: a
/// panicking mixer plays silence and fails the stream, and a mixer held by another stream (never
/// expected: a restart drops the old stream first) plays silence without advancing the clock.
fn render(fill: &Mutex<Fill>, buf: &mut [f32], channels: usize, played: &AtomicU64, health: &Health) {
    let mut guard = match fill.try_lock() {
        Ok(guard) => guard,
        Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        Err(TryLockError::WouldBlock) => {
            buf.fill(0.0);
            return;
        }
    };
    if health.failed.load(Ordering::SeqCst) || catch_unwind(AssertUnwindSafe(|| (*guard)(buf, channels))).is_err() {
        buf.fill(0.0);
        if !health.failed.swap(true, Ordering::SeqCst) {
            eprintln!("filmcraft: the audio mixer failed; playing without sound");
        }
        return;
    }
    played.fetch_add((buf.len() / channels.max(1)) as u64, Ordering::SeqCst);
}

fn stream_error(health: &Health, e: cpal::StreamError) {
    match e {
        cpal::StreamError::DeviceNotAvailable => health.lost.store(true, Ordering::SeqCst),
        cpal::StreamError::BackendSpecific { .. } => health.errored.store(true, Ordering::SeqCst),
    }
    if !health.reported.swap(true, Ordering::SeqCst) {
        eprintln!("filmcraft: audio stream error: {e}");
    }
}

fn convert_samples<T: cpal::Sample + cpal::FromSample<f32>>(input: &[f32], output: &mut [T]) {
    // Integer conversion represents [-1, 1). In particular I24 cannot represent +1 exactly.
    let upper = f32::from_bits(1.0_f32.to_bits() - 1);
    for (sample, converted) in input.iter().zip(output) {
        *converted = T::from_sample(if sample.is_finite() { sample.clamp(-1.0, upper) } else { 0.0 });
    }
}

impl AudioOut for CpalOut {
    fn start(&mut self, fill: Box<dyn FnMut(&mut [f32], usize) + Send>) -> Result<u32, String> {
        self.stop();
        self.played.store(0, Ordering::SeqCst);
        self.audible = Arc::new(Audible::default());
        self.health = Arc::new(Health::default());
        self.watchdog.reset(self.epoch.elapsed());
        let fill = Arc::new(Mutex::new(fill));
        self.open(fill.clone(), None)?;
        self.fill = Some(fill);
        Ok(self.rate)
    }
    fn stop(&mut self) {
        self.stream = None;
        self.fill = None;
    }
    fn recover(&mut self) {
        if self.stream.is_none() || self.health.failed.load(Ordering::SeqCst) {
            return;
        }
        let lost = self.health.lost.swap(false, Ordering::SeqCst);
        let errored = self.health.errored.swap(false, Ordering::SeqCst);
        match self.watchdog.check(self.epoch.elapsed(), self.played.load(Ordering::SeqCst), errored, lost) {
            Verdict::Healthy => {}
            Verdict::Restart => {
                log::warn!("the audio output stream {}; restarting it", if lost { "lost its device" } else { "stopped after an error" });
                if let Err(e) = self.restart() {
                    log::warn!("the audio output stream could not be restarted: {e}");
                    self.stream = None;
                    self.health.failed.store(true, Ordering::SeqCst);
                }
            }
            Verdict::GiveUp => {
                log::warn!("the audio output stream keeps failing ({MAX_RESTARTS} restarts within {} s)", RESTART_WINDOW.as_secs());
                self.health.failed.store(true, Ordering::SeqCst);
            }
        }
    }
    fn sample_rate(&self) -> u32 {
        self.rate
    }
    fn channels(&self) -> usize {
        self.channels as usize
    }
    fn played_frames(&self) -> Option<u64> {
        self.stream
            .as_ref()
            .filter(|_| !self.health.failed.load(Ordering::SeqCst))
            .map(|_| self.audible.frames(self.played.load(Ordering::SeqCst), self.rate, self.epoch.elapsed()))
    }
    fn devices(&self) -> AudioDevices {
        let h = host(&self.hw.device_class);
        let outputs = h.output_devices().map(|d| d.filter_map(|x| x.name().ok()).collect()).unwrap_or_default();
        let inputs = h.input_devices().map(|d| d.filter_map(|x| x.name().ok()).collect()).unwrap_or_default();
        let output_channels = output_device(&h, &self.hw.default_output, &self.fallback, &self.named)
            .and_then(|c| self.config(&c.device).ok())
            .map(|cfg| cfg.channels())
            .unwrap_or(0);
        AudioDevices { hosts: cpal::available_hosts().iter().map(|h| h.name().to_string()).collect(), inputs, outputs, output_channels }
    }
    fn configure(&mut self, hw: &AudioHardwarePrefs, document_rate: Option<u32>) {
        self.hw = hw.clone();
        self.document_rate = document_rate;
        // the rate playback mixes at must be known before `start`
        let host = host(&self.hw.device_class);
        if let Some(choice) = output_device(&host, &self.hw.default_output, &self.fallback, &self.named)
            && let Ok(cfg) = self.config(&choice.device)
        {
            self.rate = cfg.sample_rate().0;
            self.channels = cfg.channels();
            self.note = choice.fallback;
        }
    }
    fn note(&self) -> Option<String> {
        self.note.as_ref().map(Note::text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cpal::Sample;

    #[test]
    fn fallback_prefers_sound_servers_over_raw_hardware() {
        let mut names = vec!["plughw:CARD=PCH,DEV=0", "hw:CARD=PCH,DEV=0", "sysdefault:CARD=PCH", "pulse", "pipewire"];
        names.sort_by_key(|n| fallback_rank(n));
        assert_eq!(names, ["pipewire", "pulse", "sysdefault:CARD=PCH", "plughw:CARD=PCH,DEV=0", "hw:CARD=PCH,DEV=0"]);
    }

    #[test]
    fn alsa_rings_hold_four_periods_and_never_starve() {
        // cpal takes a fixed ALSA size as the whole ring: the default 512 was ~10 ms of slack
        assert_eq!(device_buffer_frames("ALSA", 512), 2048);
        assert_eq!(device_buffer_frames("ALSA", 2048), 8192);
        assert_eq!(device_buffer_frames("ALSA", 64), ALSA_MIN_RING);
        assert_eq!(device_buffer_frames("ALSA", u32::MAX), ALSA_MAX_RING);
        // other hosts take the setting as the callback size
        assert_eq!(device_buffer_frames("CoreAudio", 512), 512);
        assert_eq!(device_buffer_frames("WASAPI", 0), 1);
    }

    #[test]
    fn the_clock_is_what_is_heard_not_what_is_queued() {
        let ms = Duration::from_millis;
        let a = Audible::default();
        assert_eq!(a.frames(4800, 48_000, ms(5)), 0, "nothing is heard before the first callback");
        // 4800 frames handed over when the device asked for more; 50 ms of them were still queued
        a.on_callback(4800, ms(50), 48_000, ms(100));
        assert_eq!(a.frames(5280, 48_000, ms(100)), 2400);
        // between callbacks the clock runs on the wall clock
        assert_eq!(a.frames(5280, 48_000, ms(110)), 2880);
        // but never past what the device was given: a stalled device stops it
        assert_eq!(a.frames(5280, 48_000, ms(9_000)), 5280);
        assert_eq!(a.frames(5280, 48_000, ms(9_500)), 5280);
        // a later, longer latency estimate never moves it backwards
        a.on_callback(5280, ms(100), 48_000, ms(9_600));
        assert_eq!(a.frames(5760, 48_000, ms(9_600)), 5280);
    }

    #[test]
    fn a_broken_latency_report_cannot_stall_or_overflow_the_clock() {
        let ms = Duration::from_millis;
        let a = Audible::default();
        a.on_callback(u64::MAX, Duration::MAX, u32::MAX, Duration::MAX);
        assert_eq!(a.frames(1000, u32::MAX, Duration::MAX), 1000);
        let b = Audible::default();
        // an absurd latency is capped (2 s), so the clock still moves once that much has played
        b.on_callback(48_000 * 3, Duration::from_secs(3600), 48_000, ms(0));
        assert_eq!(b.frames(48_000 * 3, 48_000, ms(0)), 48_000);
    }

    #[test]
    fn notes_name_the_fallback_device() {
        assert!(Note::Fallback("pipewire".into()).text().contains("'pipewire'"));
        assert!(!Note::NoDevice.text().is_empty());
    }

    #[test]
    fn integer_conversion_clips_and_preserves_interleaved_channels() {
        let mut signed = [0_i16; 8];
        convert_samples(&[-2.0, 2.0, -0.5, 0.5, 0.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY], &mut signed);
        assert_eq!(signed, [i16::MIN, i16::MAX, -16_384, 16_384, 0, 0, 0, 0]);
        let mut unsigned = [0_u16; 3];
        convert_samples(&[-1.0, 0.0, 1.0], &mut unsigned);
        assert_eq!(unsigned, [0, 32_768, u16::MAX]);
    }

    #[test]
    fn full_scale_24_bit_samples_remain_representable() {
        let mut samples = [cpal::I24::EQUILIBRIUM; 3];
        convert_samples(&[-1.0, 0.0, 1.0], &mut samples);
        assert_eq!(samples.map(|s| s.inner()), [-8_388_608, 0, 8_388_607]);
    }

    #[test]
    fn stream_errors_ask_for_a_restart_instead_of_failing_playback() {
        let health = Health::default();
        stream_error(&health, cpal::StreamError::BackendSpecific { err: cpal::BackendSpecificError { description: "`alsa::poll()` returned POLLERR".into() } });
        assert!(health.errored.load(Ordering::SeqCst) && !health.lost.load(Ordering::SeqCst));
        stream_error(&health, cpal::StreamError::DeviceNotAvailable);
        assert!(health.lost.load(Ordering::SeqCst));
        assert!(!health.failed.load(Ordering::SeqCst), "a stream error alone must not silence playback");
        let output = CpalOut::new();
        assert_eq!(output.played_frames(), None, "no stream, no clock");
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn an_error_while_samples_keep_flowing_is_only_a_hiccup() {
        let mut w = Watchdog::default();
        w.reset(ms(0));
        assert_eq!(w.check(ms(10), 480, true, false), Verdict::Healthy);
        // the stream keeps consuming samples after the error: never restarted
        for (i, t) in (20..2_000).step_by(20).enumerate() {
            assert_eq!(w.check(ms(t), 480 * (i as u64 + 2), false, false), Verdict::Healthy);
        }
    }

    #[test]
    fn a_stream_that_errors_and_stops_consuming_is_restarted() {
        let mut w = Watchdog::default();
        w.reset(ms(0));
        assert_eq!(w.check(ms(10), 480, false, false), Verdict::Healthy);
        // POLLERR: the backend reports errors and no longer calls back
        assert_eq!(w.check(ms(20), 480, true, false), Verdict::Healthy);
        assert_eq!(w.check(ms(200), 480, true, false), Verdict::Healthy);
        assert_eq!(w.check(ms(310), 480, false, false), Verdict::Restart);
        // the restarted stream gets a fresh grace period
        assert_eq!(w.check(ms(400), 480, false, false), Verdict::Healthy);
        // a stall without any reported error is left to the UI's own watchdog
        assert_eq!(w.check(ms(2_000), 480, false, false), Verdict::Healthy);
    }

    #[test]
    fn a_lost_device_restarts_at_once_and_repeated_failures_give_up() {
        let mut w = Watchdog::default();
        w.reset(ms(0));
        assert_eq!(w.check(ms(5), 0, false, true), Verdict::Restart);
        assert_eq!(w.check(ms(10), 0, false, true), Verdict::Restart);
        assert_eq!(w.check(ms(15), 0, false, true), Verdict::Restart);
        assert_eq!(w.check(ms(20), 0, false, true), Verdict::GiveUp);
        // sporadic failures far apart keep being recovered
        assert_eq!(w.check(ms(10_006), 0, false, true), Verdict::Restart);
    }

    #[test]
    fn the_device_callback_never_panics_and_counts_played_frames() {
        let (played, health) = (AtomicU64::new(0), Health::default());
        let fill: Mutex<Fill> = Mutex::new(Box::new(|buf: &mut [f32], _| buf.fill(0.25)));
        let mut buf = [1.0_f32; 8];
        render(&fill, &mut buf, 2, &played, &health);
        assert_eq!((buf, played.load(Ordering::SeqCst)), ([0.25; 8], 4));
        // a mixer still held elsewhere plays silence without moving the clock
        {
            let _held = fill.lock().unwrap();
            render(&fill, &mut buf, 2, &played, &health);
        }
        assert_eq!((buf, played.load(Ordering::SeqCst)), ([0.0; 8], 4));
        // zero channels from a broken driver must not divide by zero
        render(&fill, &mut buf, 0, &played, &health);
        assert_eq!(played.load(Ordering::SeqCst), 12);
    }

    #[test]
    fn a_panicking_mixer_plays_silence_and_fails_the_stream() {
        let (played, health) = (AtomicU64::new(0), Health::default());
        let fill: Mutex<Fill> = Mutex::new(Box::new(|buf: &mut [f32], _| {
            buf.fill(0.5);
            panic!("mixer bug");
        }));
        let mut buf = [1.0_f32; 4];
        render(&fill, &mut buf, 2, &played, &health);
        assert_eq!(buf, [0.0; 4]);
        assert_eq!(played.load(Ordering::SeqCst), 0);
        assert!(health.failed.load(Ordering::SeqCst));
        // later callbacks stay silent instead of re-entering the broken mixer
        buf = [1.0; 4];
        render(&fill, &mut buf, 2, &played, &health);
        assert_eq!(buf, [0.0; 4]);
    }
}
