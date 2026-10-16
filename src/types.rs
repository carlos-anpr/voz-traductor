//! Tipos compartidos entre etapas del pipeline. Contrato fijo: no cambiar sin coordinar.

use std::time::{Duration, Instant};

/// Frecuencia de trabajo de VAD y STT.
pub const SAMPLE_RATE: u32 = 16_000;
/// Tamaño de trama que espera Silero v4 a 16 kHz (32 ms).
pub const FRAME_SAMPLES: usize = 512;
/// Frecuencia del audio traducido (la nativa de Kokoro; Edge se decodifica y remuestrea a esta).
pub const TTS_SAMPLE_RATE: u32 = 24_000;

/// Una frase detectada por el VAD: audio mono f32 a 16 kHz.
#[derive(Debug, Clone)]
pub struct Utterance {
    /// Correlativo desde 1 dentro de la sesión.
    pub id: u32,
    pub samples: Vec<f32>,
    /// Fin del audio entregado en el reloj del VAD. En un corte retrospectivo excluye el sufijo retenido.
    pub ended_at: Instant,
}

impl Utterance {
    pub fn duration(&self) -> Duration {
        Duration::from_secs_f64(self.samples.len() as f64 / SAMPLE_RATE as f64)
    }
}

/// Tiempos por etapa de una frase.
#[derive(Debug, Clone, Default)]
pub struct Timings {
    pub stt: Duration,
    pub translate: Duration,
    pub tts: Duration,
    /// Desde `Utterance::ended_at` hasta tener el audio traducido listo.
    pub total: Duration,
}

/// Resultado completo de una frase, lo que recibe la salida.
#[derive(Debug, Clone)]
pub struct Phrase {
    pub id: u32,
    /// Audio original 16 kHz mono (para guardarlo y poder comparar).
    pub original: Vec<f32>,
    pub source_text: String,
    pub target_text: String,
    /// Nombre del proveedor que tradujo la frase (`translate.providers[].name`).
    pub provider: String,
    /// Audio traducido: PCM mono f32 a `TTS_SAMPLE_RATE` (listo para guardar o reproducir).
    pub audio: Vec<f32>,
    pub timings: Timings,
}
