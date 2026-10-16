//! Segmentación con histéresis y Silero VAD embebido.
use crate::{
    config::VadConfig,
    types::{FRAME_SAMPLES, SAMPLE_RATE, Utterance},
};
use anyhow::{Context, Result, ensure};
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};
use transcribe_rs::vad::{SileroVad, Vad};

pub struct Segmenter {
    vad: SileroVad,
    cfg: VadConfig,
    pending: VecDeque<f32>,
    preroll: VecDeque<f32>,
    samples: Vec<f32>,
    speech_samples: usize,
    silence_samples: usize,
    active: bool,
    next_id: u32,
    chunking: Option<Chunking>,
}

struct FrameMark {
    end: usize,
    probability: f32,
    energy: f32,
    voice: bool,
}

struct Chunking {
    min_samples: usize,
    pause_samples: usize,
    max_samples: usize,
    frames: VecDeque<FrameMark>,
    continuing: bool,
}

impl Chunking {
    fn cut_point(&self, length: usize) -> usize {
        let earliest = self
            .min_samples
            .max(length.saturating_sub(sample_count(800)));
        let peak = self
            .frames
            .iter()
            .map(|frame| frame.energy)
            .fold(f32::EPSILON, f32::max);
        // Media de tres tramas (~96 ms): un valle sostenido es mejor frontera
        // que un cruce por cero dentro de una vocal. La energía discrimina las
        // pausas breves que Silero sigue marcando como habla con música de fondo.
        (1..self.frames.len().saturating_sub(1))
            .filter(|&i| self.frames[i].end >= earliest)
            .min_by(|&a, &b| {
                let score = |i: usize| {
                    let energy = (self.frames[i - 1].energy
                        + self.frames[i].energy
                        + self.frames[i + 1].energy)
                        / (3.0 * peak);
                    energy + self.frames[i].probability * 0.15
                };
                score(a).total_cmp(&score(b))
            })
            .map_or(length, |i| self.frames[i].end)
    }
}
fn sample_count(ms: u32) -> usize {
    (u64::from(ms) * u64::from(SAMPLE_RATE) / 1000) as usize
}
impl Segmenter {
    pub fn new(cfg: &VadConfig) -> Result<Self> {
        ensure!(
            cfg.threshold.is_finite() && (0.15..=1.0).contains(&cfg.threshold),
            "vad.threshold debe estar entre 0,15 y 1"
        );
        ensure!(
            cfg.max_speech_ms > 0 && cfg.min_speech_ms <= cfg.max_speech_ms,
            "Duraciones de voz VAD no válidas"
        );
        let bytes = include_bytes!("../assets/silero_vad_v4.onnx");
        let dir = dirs::cache_dir()
            .context("No se encuentra la carpeta de caché")?
            .join("voz-traductor");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("silero_vad_v4.onnx");
        if std::fs::metadata(&path).map(|m| m.len()).ok() != Some(bytes.len() as u64) {
            let temporary = dir.join(format!("silero-{}-{}.tmp", std::process::id(), {
                static NEXT: std::sync::atomic::AtomicUsize =
                    std::sync::atomic::AtomicUsize::new(0);
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            }));
            std::fs::write(&temporary, bytes)?;
            std::fs::rename(temporary, &path)?;
        }
        Ok(Self {
            vad: SileroVad::new(path, cfg.threshold)?,
            cfg: cfg.clone(),
            pending: VecDeque::new(),
            preroll: VecDeque::new(),
            samples: Vec::new(),
            speech_samples: 0,
            silence_samples: 0,
            active: false,
            next_id: 1,
            chunking: None,
        })
    }
    /// Solo `oir` activa este modo. `new` conserva la segmentación del micro.
    pub fn new_chunked(cfg: &VadConfig, min_ms: u32, pause_ms: u32, max_ms: u32) -> Result<Self> {
        ensure!(
            min_ms >= cfg.min_speech_ms && min_ms > 0 && max_ms > min_ms,
            "El troceo necesita min_chunk_ms >= vad.min_speech_ms y max_chunk_ms > min_chunk_ms"
        );
        ensure!(
            pause_ms > 0 && pause_ms <= min_ms,
            "chunk_pause_ms debe estar entre 1 y min_chunk_ms"
        );
        let mut segmenter = Self::new(cfg)?;
        segmenter.chunking = Some(Chunking {
            min_samples: sample_count(min_ms),
            pause_samples: sample_count(pause_ms),
            max_samples: sample_count(max_ms),
            frames: VecDeque::new(),
            continuing: false,
        });
        Ok(segmenter)
    }
    pub fn is_speaking(&self) -> bool {
        self.active
    }
    pub fn push_frame(&mut self, frame: &[f32]) -> Result<Option<Utterance>> {
        ensure!(
            frame.len() == FRAME_SAMPLES,
            "El VAD necesita tramas de {FRAME_SAMPLES} muestras"
        );
        // transcribe-rs expone ventanas de 480; conservamos el sobrante entre tramas de 512.
        self.pending.extend(frame);
        let mut probability = 0.0_f32;
        while self.pending.len() >= self.vad.frame_size() {
            let window: Vec<_> = self.pending.drain(..self.vad.frame_size()).collect();
            probability = probability.max(self.vad.speech_probability(&window)?);
        }
        Ok(self.push_probability(frame, probability))
    }
    fn push_probability(&mut self, frame: &[f32], probability: f32) -> Option<Utterance> {
        let voice = probability
            >= if self.active {
                self.cfg.threshold - 0.15
            } else {
                self.cfg.threshold
            };
        if !self.active && voice {
            self.active = true;
            self.samples.extend(self.preroll.drain(..));
        }
        if self.active {
            self.samples.extend_from_slice(frame);
            if let Some(chunking) = &mut self.chunking {
                chunking.frames.push_back(FrameMark {
                    end: self.samples.len(),
                    probability,
                    energy: frame.iter().map(|sample| sample * sample).sum::<f32>()
                        / frame.len() as f32,
                    voice,
                });
            }
            if voice {
                self.speech_samples += frame.len();
                self.silence_samples = 0;
            } else {
                self.silence_samples += frame.len();
            }
            if !voice && self.silence_samples >= sample_count(self.cfg.silence_ms) {
                return self.flush();
            }
            if let Some(chunking) = &self.chunking {
                if self.samples.len() >= chunking.min_samples
                    && self.silence_samples >= chunking.pause_samples
                {
                    return self.flush();
                }
                if self.samples.len() >= chunking.max_samples {
                    return Some(self.split_chunk(chunking.cut_point(self.samples.len())));
                }
            } else if self.samples.len() >= sample_count(self.cfg.max_speech_ms) {
                return self.flush();
            }
        } else {
            self.preroll.extend(frame);
            let excess = self
                .preroll
                .len()
                .saturating_sub(sample_count(self.cfg.preroll_ms));
            self.preroll.drain(..excess);
        }
        None
    }
    fn split_chunk(&mut self, cut: usize) -> Utterance {
        let tail = self.samples.split_off(cut);
        let samples = std::mem::replace(&mut self.samples, tail);
        let ended_at = Instant::now()
            - Duration::from_secs_f64(self.samples.len() as f64 / f64::from(SAMPLE_RATE));
        let chunking = self.chunking.as_mut().expect("troceo activado");
        chunking.frames.retain(|frame| frame.end > cut);
        for frame in &mut chunking.frames {
            frame.end -= cut;
        }
        chunking.continuing = true;
        self.speech_samples =
            chunking.frames.iter().filter(|frame| frame.voice).count() * FRAME_SAMPLES;
        self.silence_samples = chunking
            .frames
            .iter()
            .rev()
            .take_while(|frame| !frame.voice)
            .count()
            * FRAME_SAMPLES;
        // Conservamos activo el VAD y todo el sufijo: no hay un nuevo preroll
        // ni se pierde el comienzo de la siguiente palabra al cortar por máximo.
        let utterance = Utterance {
            id: self.next_id,
            samples,
            ended_at,
        };
        self.next_id += 1;
        utterance
    }
    pub fn flush(&mut self) -> Option<Utterance> {
        let continuation = self
            .chunking
            .as_ref()
            .is_some_and(|chunking| chunking.continuing);
        let keep = self.active
            && (self.speech_samples >= sample_count(self.cfg.min_speech_ms)
                || (continuation && self.speech_samples > 0));
        if let Some(chunking) = &mut self.chunking {
            chunking.frames.clear();
            chunking.continuing = false;
        }
        self.active = false;
        self.speech_samples = 0;
        self.silence_samples = 0;
        let samples = std::mem::take(&mut self.samples);
        if !keep {
            return None;
        }
        let utterance = Utterance {
            id: self.next_id,
            samples,
            ended_at: Instant::now(),
        };
        self.next_id += 1;
        Some(utterance)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn chunk_cuts_at_short_pause_only_after_minimum() {
        let cfg = VadConfig {
            preroll_ms: 0,
            ..VadConfig::default()
        };
        let mut vad = Segmenter::new_chunked(&cfg, 1500, 192, 3500).unwrap();
        let voice = [0.1; FRAME_SAMPLES];
        let silence = [0.0; FRAME_SAMPLES];
        for _ in 0..20 {
            assert!(vad.push_probability(&voice, 0.9).is_none());
        }
        for _ in 0..6 {
            assert!(vad.push_probability(&silence, 0.1).is_none());
        }
        for _ in 0..25 {
            assert!(vad.push_probability(&voice, 0.9).is_none());
        }
        for _ in 0..5 {
            assert!(vad.push_probability(&silence, 0.1).is_none());
        }
        let chunk = vad.push_probability(&silence, 0.1).unwrap();
        assert_eq!(chunk.samples.len(), 57 * FRAME_SAMPLES);
        assert!(!vad.is_speaking());
        assert!(vad.flush().is_none());
    }

    #[test]
    fn forced_chunks_preserve_every_sample_and_keep_vad_active() {
        let cfg = VadConfig {
            preroll_ms: 64,
            ..VadConfig::default()
        };
        let mut vad = Segmenter::new_chunked(&cfg, 1500, 200, 3500).unwrap();
        let mut input = Vec::new();
        let mut output = Vec::new();
        // Preroll identificado, que debe aparecer exactamente una vez.
        for value in [-0.02, -0.01] {
            let frame = [value; FRAME_SAMPLES];
            input.extend(frame);
            assert!(vad.push_probability(&frame, 0.0).is_none());
        }
        let mut cuts = 0;
        for i in 0..330 {
            let frame = [if (87..90).contains(&(i % 100)) {
                0.001
            } else {
                0.1 + i as f32 / 10000.0
            }; FRAME_SAMPLES];
            input.extend(frame);
            if let Some(chunk) = vad.push_probability(&frame, 0.95) {
                cuts += 1;
                assert!(vad.is_speaking());
                assert!(chunk.duration() >= Duration::from_millis(1500));
                assert!(chunk.duration() <= Duration::from_millis(3532));
                let tail =
                    Duration::from_secs_f64(vad.samples.len() as f64 / f64::from(SAMPLE_RATE));
                assert!(chunk.ended_at.elapsed() >= tail);
                assert!(chunk.ended_at.elapsed() < tail + Duration::from_millis(100));
                output.extend(chunk.samples);
            }
        }
        output.extend(vad.flush().unwrap().samples);
        assert!(cuts >= 3);
        assert_eq!(
            input, output,
            "sin muestras perdidas, duplicadas ni reordenadas"
        );
    }

    #[test]
    fn forced_cut_uses_energy_valley_and_preserves_short_final_tail() {
        let cfg = VadConfig {
            preroll_ms: 0,
            ..VadConfig::default()
        };
        let mut vad = Segmenter::new_chunked(&cfg, 1500, 200, 3500).unwrap();
        let mut first = None;
        for i in 0..110 {
            // Silero sigue diciendo voz; la energía cae durante 96 ms.
            let frame = [if (104..107).contains(&i) { 0.001 } else { 0.2 }; FRAME_SAMPLES];
            first = vad.push_probability(&frame, 0.99).or(first);
        }
        let first = first.unwrap();
        assert_eq!(first.samples.len(), 106 * FRAME_SAMPLES);
        let tail = vad.flush().expect("no descartar una continuación corta");
        assert!(tail.duration() < Duration::from_millis(cfg.min_speech_ms.into()));
        assert_eq!(
            first.samples.len() + tail.samples.len(),
            110 * FRAME_SAMPLES
        );
        assert_eq!(tail.id, first.id + 1);
    }

    #[test]
    fn invalid_chunk_settings_are_rejected() {
        let cfg = VadConfig::default();
        for (min, pause, max) in [
            (0, 200, 3500),
            (1500, 0, 3500),
            (1500, 1600, 3500),
            (1500, 200, 1500),
        ] {
            assert!(Segmenter::new_chunked(&cfg, min, pause, max).is_err());
        }
    }

    #[test]
    #[ignore = "Auditoría local de todos los trozos con el WAV inglés de medición"]
    fn benchmark_chunk_durations() {
        let path = std::env::var_os("VOZ_CHUNK_FIXTURE").expect("Indica VOZ_CHUNK_FIXTURE");
        let samples = crate::audio::read_wav_16k(std::path::Path::new(&path)).unwrap();
        let cfg = VadConfig {
            silence_ms: 450,
            max_speech_ms: 8000,
            ..VadConfig::default()
        };
        let mut results = Vec::new();
        let mut stt = crate::stt::Stt::load(&crate::config::SttConfig::default()).unwrap();
        for chunked in [false, true] {
            let mut vad = if chunked {
                Segmenter::new_chunked(&cfg, 1500, 200, 3500).unwrap()
            } else {
                Segmenter::new(&cfg).unwrap()
            };
            let mut chunks = Vec::new();
            for frame in samples.chunks(FRAME_SAMPLES) {
                let mut padded = [0.0; FRAME_SAMPLES];
                padded[..frame.len()].copy_from_slice(frame);
                if let Some(utterance) = vad.push_frame(&padded).unwrap() {
                    chunks.push(utterance);
                }
            }
            if let Some(utterance) = vad.flush() {
                chunks.push(utterance);
            }
            let records = chunks.iter().map(|chunk| {
                let text = stt.transcribe(&chunk.samples).unwrap();
                serde_json::json!({"id":chunk.id,"seconds":chunk.duration().as_secs_f64(),"text":text})
            }).collect::<Vec<_>>();
            results.push(serde_json::json!({"chunked":chunked,"chunks":records}));
        }
        let report = serde_json::to_string_pretty(&results).unwrap();
        println!("{report}");
        if let Some(path) = std::env::var_os("VOZ_CHUNK_RESULT") {
            std::fs::write(path, report).unwrap();
        }
    }
    #[test]
    fn silence_never_produces_a_phrase() {
        let mut vad = Segmenter::new(&VadConfig::default()).unwrap();
        for _ in 0..100 {
            assert!(vad.push_frame(&[0.0; FRAME_SAMPLES]).unwrap().is_none());
        }
        assert!(vad.flush().is_none());
    }
    #[test]
    fn hysteresis_preroll_and_short_speech() {
        let cfg = VadConfig {
            silence_ms: 64,
            min_speech_ms: 64,
            preroll_ms: 32,
            ..VadConfig::default()
        };
        let mut vad = Segmenter::new(&cfg).unwrap();
        let frame = [0.1; FRAME_SAMPLES];
        assert!(vad.push_probability(&frame, 0.0).is_none());
        assert!(vad.push_probability(&frame, 0.6).is_none());
        assert!(vad.push_probability(&frame, 0.4).is_none());
        assert!(vad.push_probability(&frame, 0.2).is_none());
        let phrase = vad.push_probability(&frame, 0.2).unwrap();
        assert_eq!(phrase.samples.len(), 5 * FRAME_SAMPLES);
        assert_eq!(phrase.id, 1);
        vad.push_probability(&frame, 0.8);
        assert!(vad.flush().is_none());
    }
    #[test]
    fn maximum_duration_and_final_flush() {
        let cfg = VadConfig {
            max_speech_ms: 64,
            min_speech_ms: 32,
            ..VadConfig::default()
        };
        let mut vad = Segmenter::new(&cfg).unwrap();
        assert!(vad.push_probability(&[0.1; FRAME_SAMPLES], 0.9).is_none());
        assert!(vad.push_probability(&[0.1; FRAME_SAMPLES], 0.9).is_some());
        vad.push_probability(&[0.1; FRAME_SAMPLES], 0.9);
        assert_eq!(vad.flush().unwrap().id, 2);
    }
}
