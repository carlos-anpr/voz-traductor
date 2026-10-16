//! Síntesis y reproducción de doblaje desacopladas, con cola acotada por segundos.
use crate::{
    config::{Config, TtsConfig, TtsEngine},
    ducking::{Ducker, VolumeGuard},
    output::DubWriter,
    playback::DubOutput,
    tts::Tts,
    types::TTS_SAMPLE_RATE,
};
use anyhow::{Result, anyhow};
use std::{
    collections::VecDeque,
    fs::File,
    io::Write,
    path::Path,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
const MAX_QUEUE: f64 = 6.0;
struct Job {
    id: u32,
    text: String,
    estimate: f64,
}
struct Audio {
    id: u32,
    samples: Vec<f32>,
    speed: f32,
}
impl Audio {
    fn duration(&self) -> f64 {
        self.samples.len() as f64 / f64::from(TTS_SAMPLE_RATE)
    }
}
struct Inflight {
    id: u32,
    estimate: f64,
    discarded: bool,
}
#[derive(Clone, Default)]
pub struct DubStats {
    pub queue_mean: f64,
    pub queue_max: f64,
    pub discarded: usize,
    pub cancelled: usize,
    pub played: usize,
    pub speeds: Vec<f32>,
}
struct State {
    pending: VecDeque<Job>,
    ready: VecDeque<Audio>,
    inflight: Option<Inflight>,
    playing_until: Option<Instant>,
    closed: bool,
    synth_done: bool,
    stats: DubStats,
    area: f64,
    started: Instant,
    last_sample: Instant,
    last_level: f64,
}
impl State {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            pending: VecDeque::new(),
            ready: VecDeque::new(),
            inflight: None,
            playing_until: None,
            closed: false,
            synth_done: false,
            stats: DubStats::default(),
            area: 0.0,
            started: now,
            last_sample: now,
            last_level: 0.0,
        }
    }
    fn queued(&self, now: Instant) -> f64 {
        self.pending.iter().map(|job| job.estimate).sum::<f64>()
            + self.ready.iter().map(Audio::duration).sum::<f64>()
            + self
                .inflight
                .as_ref()
                .filter(|job| !job.discarded)
                .map_or(0.0, |job| job.estimate)
            + self.playing_until.map_or(0.0, |until| {
                until.saturating_duration_since(now).as_secs_f64()
            })
    }
    fn sample(&mut self, now: Instant) {
        self.area += self.last_level * now.duration_since(self.last_sample).as_secs_f64();
        self.last_sample = now;
        self.last_level = self.queued(now);
        self.stats.queue_max = self.stats.queue_max.max(self.last_level);
    }
    fn limit(&mut self, now: Instant) {
        while self.queued(now) > MAX_QUEUE {
            let oldest = self
                .pending
                .front()
                .map(|job| job.id)
                .into_iter()
                .chain(self.ready.front().map(|audio| audio.id))
                .chain(
                    self.inflight
                        .as_ref()
                        .filter(|job| !job.discarded)
                        .map(|job| job.id),
                )
                .min();
            let Some(id) = oldest else {
                break;
            }; // nunca interrumpir lo que ya suena
            if self.pending.front().is_some_and(|job| job.id == id) {
                self.pending.pop_front();
            } else if self.ready.front().is_some_and(|audio| audio.id == id) {
                self.ready.pop_front();
            } else if let Some(job) = &mut self.inflight {
                job.discarded = true;
            }
            self.stats.discarded += 1;
            eprintln!(
                "Doblaje #{id} descartado: cola superior a {MAX_QUEUE:.0} s (subtítulo conservado)"
            );
        }
    }
    fn speed(&self, now: Instant, max: f32) -> f32 {
        let pressure = ((self.queued(now) - 1.5) / 4.5).clamp(0.0, 1.0) as f32;
        1.0 + (max - 1.0) * pressure
    }
}
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    stop: Arc<AtomicBool>,
}
impl Shared {
    fn worker(&self, f: impl FnOnce() -> Result<()>) -> Result<()> {
        // También despierta a las otras etapas si este hilo sufre un panic.
        struct Guard<'a>(&'a Shared, bool);
        impl Drop for Guard<'_> {
            fn drop(&mut self) {
                if self.1 {
                    self.0.stop.store(true, Ordering::SeqCst);
                    self.0.changed.notify_all();
                }
            }
        }
        let mut guard = Guard(self, true);
        let result = f();
        if result.is_ok() {
            guard.1 = false;
        }
        result
    }
}
pub struct Dubbing {
    shared: Arc<Shared>,
    workers: Vec<JoinHandle<Result<()>>>,
}
fn voice_config(cfg: &Config) -> Result<TtsConfig> {
    let mut tts = cfg.tts.clone();
    tts.voice = cfg
        .oir
        .dub_voice
        .clone()
        .unwrap_or_else(|| match tts.engine {
            TtsEngine::Kokoro => "em_alex".into(),
            TtsEngine::Edge => "es-ES-AlvaroNeural".into(),
        });
    let spanish = match tts.engine {
        TtsEngine::Kokoro => tts.voice.starts_with('e') && !tts.voice.starts_with("es-"),
        TtsEngine::Edge => tts.voice.starts_with("es-"),
    };
    anyhow::ensure!(
        spanish,
        "Selecciona una voz española del motor configurado con --voz-es"
    );
    anyhow::ensure!(
        cfg.oir.target_lang.eq_ignore_ascii_case("Spanish"),
        "El doblaje español necesita oir.target_lang = Spanish"
    );
    tts.speed = 1.0;
    tts.validate()?;
    Ok(tts)
}
impl Dubbing {
    pub fn start(
        cfg: &Config,
        dir: &Path,
        stop: Arc<AtomicBool>,
        guard: Arc<Mutex<VolumeGuard>>,
    ) -> Result<Self> {
        cfg.oir.validate_dubbing()?;
        let output = DubOutput::new(cfg.playback.monitor_device.as_deref())?;
        let tts_cfg = voice_config(cfg)?;
        println!(
            "Doblaje español: {} · velocidad 1,00–{:.2}",
            tts_cfg.voice, cfg.oir.max_speed
        );
        let tts = Tts::new(&tts_cfg)?;
        let writer = DubWriter::create(&dir.join("doblaje.wav"))?;
        let log = File::create(dir.join("doblaje.jsonl"))?;
        let ducker = if cfg.oir.duck_original {
            Some(Ducker::new(
                cfg.oir
                    .app
                    .as_deref()
                    .ok_or_else(|| anyhow!("Falta --app"))?,
                cfg.oir.duck_volume,
                &dir.join("volumen-original.jsonl"),
                guard,
            )?)
        } else {
            None
        };
        let shared = Arc::new(Shared {
            state: Mutex::new(State::new()),
            changed: Condvar::new(),
            stop,
        });
        let synth_shared = shared.clone();
        let max_speed = cfg.oir.max_speed;
        let synth = thread::Builder::new()
            .name("doblaje-sintesis".into())
            .spawn(move || {
                synth_shared.worker(|| synthesize(&synth_shared, tts, writer, log, max_speed))
            })?;
        let play_shared = shared.clone();
        let player = match thread::Builder::new()
            .name("doblaje-salida".into())
            .spawn(move || play_shared.worker(|| play(&play_shared, output, ducker)))
        {
            Ok(player) => player,
            Err(error) => {
                shared.stop.store(true, Ordering::SeqCst);
                shared.changed.notify_all();
                let _ = synth.join();
                return Err(error.into());
            }
        };
        Ok(Self {
            shared,
            workers: vec![synth, player],
        })
    }
    pub fn enqueue(&self, id: u32, text: &str) {
        let mut state = self.shared.state.lock().unwrap();
        let now = Instant::now();
        state.sample(now);
        if self.shared.stop.load(Ordering::Relaxed) {
            state.stats.cancelled += 1;
            return;
        }
        state.pending.push_back(Job {
            id,
            text: text.into(),
            estimate: (text.chars().count() as f64 / 15.0).max(0.2),
        });
        state.limit(now);
        state.sample(now);
        self.shared.changed.notify_all();
    }
    pub fn finish(mut self) -> (DubStats, Result<()>) {
        self.join()
    }
    fn join(&mut self) -> (DubStats, Result<()>) {
        {
            let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            state.closed = true;
            self.shared.changed.notify_all();
        }
        let mut result = Ok(());
        for worker in self.workers.drain(..) {
            if let Err(error) = worker
                .join()
                .map_err(|_| anyhow!("Un hilo de doblaje terminó inesperadamente"))
                .and_then(|r| r)
            {
                result = Err(error);
            }
        }
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.sample(Instant::now());
        let mut stats = state.stats.clone();
        stats.queue_mean = state.area / state.started.elapsed().as_secs_f64().max(0.001);
        (stats, result)
    }
}
impl Drop for Dubbing {
    fn drop(&mut self) {
        if !self.workers.is_empty() {
            self.shared.stop.store(true, Ordering::SeqCst);
            let (_, result) = self.join();
            if let Err(error) = result {
                eprintln!("Error cerrando doblaje: {error:#}");
            }
        }
    }
}
fn synthesize(
    shared: &Shared,
    mut tts: Tts,
    mut writer: DubWriter,
    mut log: File,
    max_speed: f32,
) -> Result<()> {
    loop {
        let (job, speed) = {
            let mut state = shared.state.lock().unwrap();
            while state.pending.is_empty() && !state.closed && !shared.stop.load(Ordering::Relaxed)
            {
                state = shared
                    .changed
                    .wait_timeout(state, Duration::from_millis(100))
                    .unwrap()
                    .0;
            }
            if shared.stop.load(Ordering::Relaxed) || (state.closed && state.pending.is_empty()) {
                break;
            }
            let now = Instant::now();
            state.sample(now);
            let job = state.pending.pop_front().expect("texto pendiente");
            let speed = state.speed(now, max_speed);
            state.inflight = Some(Inflight {
                id: job.id,
                estimate: job.estimate / f64::from(speed),
                discarded: false,
            });
            state.sample(now);
            (job, speed)
        };
        let started = Instant::now();
        tts.set_speed(speed)?;
        let audio = tts.synthesize(&job.text)?;
        writer.append(&audio)?;
        let duration = audio.len() as f64 / f64::from(TTS_SAMPLE_RATE);
        let mut state = shared.state.lock().unwrap();
        let now = Instant::now();
        state.sample(now);
        let discarded = state.inflight.take().is_some_and(|job| job.discarded);
        if !discarded && shared.stop.load(Ordering::Relaxed) {
            state.stats.cancelled += 1;
        }
        if !discarded && !shared.stop.load(Ordering::Relaxed) {
            state.ready.push_back(Audio {
                id: job.id,
                samples: audio,
                speed,
            });
            state.limit(now);
        }
        state.stats.speeds.push(speed);
        state.sample(now);
        let queue = state.queued(now);
        println!(
            "[doblaje #{}] voz {:.3} s · audio {:.3} s · velocidad {:.3} · cola {:.3} s",
            job.id,
            started.elapsed().as_secs_f64(),
            duration,
            speed,
            queue
        );
        writeln!(
            log,
            "{}",
            serde_json::json!({"id":job.id,"speed":speed,"audio_seconds":duration,"synthesis_seconds":started.elapsed().as_secs_f64(),"queue_seconds":queue,"discarded_inflight":discarded})
        )?;
        log.flush()?;
        shared.changed.notify_all();
    }
    let mut state = shared.state.lock().unwrap();
    state.synth_done = true;
    shared.changed.notify_all();
    drop(state);
    writer.finish()
}
fn play(shared: &Shared, mut output: DubOutput, mut ducker: Option<Ducker>) -> Result<()> {
    loop {
        if shared.stop.load(Ordering::Relaxed) {
            let mut state = shared.state.lock().unwrap();
            state.stats.cancelled += state.pending.len() + state.ready.len();
            state.pending.clear();
            state.ready.clear();
            state.playing_until = None;
            break;
        }
        if let Some(ducker) = &mut ducker
            && let Err(error) = ducker.tick(false)
        {
            if shared.stop.load(Ordering::Relaxed) {
                break;
            }
            return Err(error);
        }
        let next = {
            let mut state = shared.state.lock().unwrap();
            let now = Instant::now();
            state.sample(now);
            let next = state.ready.pop_front();
            if let Some(audio) = &next {
                state.playing_until = Some(now + Duration::from_secs_f64(audio.duration()));
            }
            if next.is_none() && state.synth_done {
                break;
            }
            state.sample(now);
            next
        };
        if let Some(audio) = next {
            println!("[reproduciendo #{}] velocidad {:.3}", audio.id, audio.speed);
            let done = output.play(&audio.samples, &shared.stop, || {
                shared.state.lock().unwrap().sample(Instant::now());
                if let Some(ducker) = &mut ducker {
                    ducker.tick(true)?;
                }
                Ok(())
            })?;
            let mut state = shared.state.lock().unwrap();
            state.sample(Instant::now());
            state.playing_until = None;
            if done {
                state.stats.played += 1;
            } else {
                state.stats.cancelled += 1;
            }
            state.sample(Instant::now());
        } else {
            thread::sleep(Duration::from_millis(20));
        }
    }
    // Drop restaura inmediatamente si la parada procede de una señal o error.
    drop(ducker);
    Ok(())
}

#[cfg(test)]
#[path = "dubbing_tests.rs"]
mod tests;
