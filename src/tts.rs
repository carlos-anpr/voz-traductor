//! Fachada PCM de los motores Kokoro local y Microsoft Edge.

use crate::{
    config::{TtsConfig, TtsEngine},
    kokoro::Kokoro,
    types::TTS_SAMPLE_RATE,
};
use anyhow::{Context, Result, ensure};
use rubato::{Fft, FixedSync, Resampler, audioadapter_buffers::direct::InterleavedSlice};
use std::io::Cursor;
use std::net::TcpStream;
use std::time::{Duration, Instant};

const MP3_FORMAT: &str = "audio-24khz-48kbitrate-mono-mp3";
/// El servidor cierra el websocket tras un rato sin uso; reconectar antes evita
/// leer de un socket muerto (el crate no expone timeouts de lectura).
const IDLE_RECONNECT_SECS: u64 = 30;

struct Edge {
    client: msedge_tts::tts::client::MSEdgeTTSClient<TcpStream>,
    speech: msedge_tts::tts::SpeechConfig,
    cfg: TtsConfig,
    last_used: Instant,
}

impl Edge {
    /// Conecta ya (precalienta) para que la primera frase no pague el ~1 s de conexión.
    pub fn new(cfg: &TtsConfig) -> anyhow::Result<Self> {
        let speech = msedge_tts::tts::SpeechConfig {
            voice_name: cfg.voice.clone(),
            audio_format: MP3_FORMAT.to_owned(),
            pitch: 0,
            rate: ((cfg.speed - 1.0) * 100.0).round().clamp(-50.0, 100.0) as i32,
            volume: 0,
        };
        let client = connect()?;
        Ok(Self {
            client,
            speech,
            cfg: cfg.clone(),
            last_used: Instant::now(),
        })
    }

    /// Devuelve MP3 (`audio-24khz-48kbitrate-mono-mp3`). Si la conexión se cayó, reconecta y reintenta una vez.
    pub fn synthesize(&mut self, text: &str) -> anyhow::Result<Vec<u8>> {
        if text.trim().is_empty() {
            return Err(anyhow::anyhow!("texto vacío: nada que sintetizar"));
        }
        if self.last_used.elapsed() > Duration::from_secs(IDLE_RECONNECT_SECS) {
            self.client = connect()?;
            self.last_used = Instant::now();
        }
        let ssml_text = escape_ssml(text);
        match self.synthesize_once(&ssml_text) {
            Ok(bytes) => {
                self.last_used = Instant::now();
                Ok(bytes)
            }
            Err(_) => {
                self.client = connect()?;
                let bytes = self.synthesize_once(&ssml_text)?;
                self.last_used = Instant::now();
                Ok(bytes)
            }
        }
    }

    /// Llamar en los ratos sin frases: renueva la conexión antes de que caduque, para que
    /// la siguiente frase no pague la reconexión.
    pub fn keep_warm(&mut self) {
        if self.last_used.elapsed() > Duration::from_secs(IDLE_RECONNECT_SECS - 10)
            && let Ok(client) = connect()
        {
            self.client = client;
            self.last_used = Instant::now();
        }
    }
    fn synthesize_once(&mut self, ssml_text: &str) -> anyhow::Result<Vec<u8>> {
        let audio = self
            .client
            .synthesize(ssml_text, &self.speech)
            .map_err(|e| {
                anyhow::anyhow!("error al sintetizar con la voz {}: {e}", self.cfg.voice)
            })?;
        if audio.audio_bytes.is_empty() {
            return Err(anyhow::anyhow!(
                "la síntesis con la voz {} devolvió 0 bytes",
                self.cfg.voice
            ));
        }
        Ok(audio.audio_bytes)
    }
}

enum Engine {
    Kokoro(Box<Kokoro>),
    Edge(Box<Edge>),
}
pub struct Tts {
    engine: Engine,
}
impl Tts {
    pub fn new(cfg: &TtsConfig) -> Result<Self> {
        cfg.validate()?;
        let engine = match cfg.engine {
            TtsEngine::Kokoro => Engine::Kokoro(Box::new(Kokoro::load(cfg)?)),
            TtsEngine::Edge => Engine::Edge(Box::new(Edge::new(cfg)?)),
        };
        Ok(Self { engine })
    }
    /// PCM mono f32 a 24 kHz, independiente del motor configurado.
    pub fn synthesize(&mut self, text: &str) -> Result<Vec<f32>> {
        ensure!(!text.trim().is_empty(), "Texto vacío: nada que sintetizar");
        match &mut self.engine {
            Engine::Kokoro(kokoro) => kokoro.synthesize(text),
            Engine::Edge(edge) => decode_mp3(&edge.synthesize(text)?),
        }
    }
    pub fn keep_warm(&mut self) {
        if let Engine::Edge(edge) = &mut self.engine {
            edge.keep_warm();
        }
    }
    pub fn set_speed(&mut self, speed: f32) -> Result<()> {
        ensure!(
            speed.is_finite() && (0.5..=2.0).contains(&speed),
            "Velocidad de síntesis fuera de rango"
        );
        match &mut self.engine {
            Engine::Kokoro(kokoro) => kokoro.set_speed(speed)?,
            Engine::Edge(edge) => {
                edge.cfg.speed = speed;
                edge.speech.rate = ((speed - 1.0) * 100.0).round() as i32;
            }
        }
        Ok(())
    }
}
fn decode_mp3(bytes: &[u8]) -> Result<Vec<f32>> {
    let mut decoder = minimp3::Decoder::new(Cursor::new(bytes));
    let mut audio = Vec::new();
    let mut sample_rate = None;
    loop {
        match decoder.next_frame() {
            Ok(frame) => {
                ensure!(
                    frame.channels > 0 && frame.sample_rate > 0,
                    "Formato MP3 de Edge no válido"
                );
                ensure!(
                    sample_rate.is_none_or(|rate| rate == frame.sample_rate),
                    "El MP3 cambió de frecuencia durante la síntesis"
                );
                sample_rate = Some(frame.sample_rate);
                audio.extend(frame.data.chunks_exact(frame.channels).map(|channel| {
                    channel.iter().map(|s| f32::from(*s) / 32768.0).sum::<f32>()
                        / frame.channels as f32
                }));
            }
            Err(minimp3::Error::Eof) => break,
            Err(minimp3::Error::SkippedData) => continue,
            Err(error) => return Err(error).context("No se puede decodificar el MP3 de Edge"),
        }
    }
    ensure!(
        !audio.is_empty(),
        "Edge devolvió un MP3 sin muestras de audio"
    );
    let rate = sample_rate.context("El MP3 de Edge no tiene frecuencia")? as u32;
    if rate == TTS_SAMPLE_RATE {
        return Ok(audio);
    }
    let mut resampler = Fft::<f32>::new(
        rate as usize,
        TTS_SAMPLE_RATE as usize,
        1024,
        1,
        FixedSync::Input,
    )?;
    let input = InterleavedSlice::new(&audio, 1, audio.len())?;
    Ok(resampler
        .process_all(&input, audio.len(), None)?
        .take_data())
}

pub fn list_voices(cfg: &TtsConfig, locale_prefix: &str) -> Result<Vec<(String, String, String)>> {
    if cfg.engine == TtsEngine::Edge {
        return edge_voices(locale_prefix);
    }
    let prefix = locale_prefix.to_lowercase();
    Ok(crate::kokoro::list_voices(&cfg.kokoro_dir)?
        .into_iter()
        .filter_map(|name| {
            let locale = match name.as_bytes().first()? {
                b'a' => "en-US",
                b'b' => "en-GB",
                b'e' => "es",
                b'f' => "fr-FR",
                b'h' => "hi",
                b'i' => "it",
                b'j' => "ja",
                b'p' => "pt-BR",
                b'z' => "zh",
                _ => "",
            };
            if !locale.to_lowercase().starts_with(&prefix) {
                return None;
            }
            let gender = match name.as_bytes().get(1) {
                Some(b'f') => "Femenina",
                Some(b'm') => "Masculina",
                _ => "",
            };
            Some((name, gender.to_owned(), locale.to_owned()))
        })
        .collect())
}

fn connect() -> anyhow::Result<msedge_tts::tts::client::MSEdgeTTSClient<TcpStream>> {
    msedge_tts::tts::client::connect()
        .map_err(|e| anyhow::anyhow!("no se pudo conectar con la voz de Edge: {e}"))
}

/// Lista de voces disponibles cuyo locale empieza por `locale_prefix` (p. ej. "en"): (nombre corto, género, locale).
fn edge_voices(locale_prefix: &str) -> anyhow::Result<Vec<(String, String, String)>> {
    let voices = msedge_tts::voice::get_voices_list()
        .map_err(|e| anyhow::anyhow!("no se pudo obtener la lista de voces: {e}"))?;
    let prefix = locale_prefix.to_lowercase();
    let mut out: Vec<(String, String, String)> = voices
        .iter()
        .filter_map(|v| {
            let locale = v.locale.clone().unwrap_or_default();
            if !prefix.is_empty() && !locale.to_lowercase().starts_with(&prefix) {
                return None;
            }
            let short = v.short_name.clone().unwrap_or_else(|| v.name.clone());
            let gender = v.gender.clone().unwrap_or_default();
            Some((short, gender, locale))
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

pub(crate) fn escape_ssml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_protege_ssml() {
        assert_eq!(
            escape_ssml("pan & queso <fresco>"),
            "pan &amp; queso &lt;fresco&gt;"
        );
        assert_eq!(escape_ssml("sin nada raro"), "sin nada raro");
    }

    #[test]
    fn mp3_stereo_resamples_to_24k_and_rejects_invalid_audio() {
        let bytes = include_bytes!("../tests/fixtures/tono-estereo-44100.mp3");
        let audio = decode_mp3(bytes).unwrap();
        assert!((2800..6000).contains(&audio.len()));
        assert!(audio.iter().all(|s| s.is_finite()));
        assert!(audio.iter().any(|s| s.abs() > 0.05));
        assert!(decode_mp3(b"no es un mp3").is_err());
        assert!(decode_mp3(&[]).is_err());
    }
    #[test]
    fn formato_mp3_constante() {
        assert_eq!(MP3_FORMAT, "audio-24khz-48kbitrate-mono-mp3");
    }
}
