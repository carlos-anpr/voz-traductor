//! Transcripción local con Parakeet TDT.
use crate::{config::SttConfig, types::SAMPLE_RATE};
use anyhow::{Context, Result, ensure};
use transcribe_rs::onnx::{
    Quantization,
    parakeet::{ParakeetModel, ParakeetParams},
};

pub struct Stt {
    model: ParakeetModel,
}
impl Stt {
    pub fn load(cfg: &SttConfig) -> Result<Self> {
        ensure!(
            cfg.model_dir.is_dir(),
            "No se encuentra el modelo STT en {}. Configura stt.model_dir con la carpeta del modelo Parakeet.",
            cfg.model_dir.display()
        );
        let model =
            ParakeetModel::load(&cfg.model_dir, &Quantization::Int8).with_context(|| {
                format!(
                    "No se puede cargar el modelo en {}; revisa stt.model_dir",
                    cfg.model_dir.display()
                )
            })?;
        let mut stt = Self { model };
        stt.transcribe(&vec![0.0; SAMPLE_RATE as usize])
            .context("Error al calentar el modelo STT")?;
        Ok(stt)
    }
    pub fn transcribe(&mut self, samples: &[f32]) -> Result<String> {
        if samples.is_empty() {
            return Ok(String::new());
        }
        Ok(self
            .model
            .transcribe_with(samples, &ParakeetParams::default())?
            .text
            .trim()
            .to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "Auditoría de una grabación de doblaje, sin síntesis ni reproducción"]
    fn benchmark_dub_recording() {
        let path = std::env::var_os("VOZ_DUB_RECORDING").expect("Indica VOZ_DUB_RECORDING");
        let audio = crate::audio::read_wav_16k(std::path::Path::new(&path)).unwrap();
        let cfg = crate::config::VadConfig {
            silence_ms: 300,
            min_speech_ms: 200,
            max_speech_ms: 5000,
            ..Default::default()
        };
        let mut vad = crate::vad::Segmenter::new(&cfg).unwrap();
        let mut chunks = Vec::new();
        for chunk in audio.chunks(crate::types::FRAME_SAMPLES) {
            let mut frame = [0.0; crate::types::FRAME_SAMPLES];
            frame[..chunk.len()].copy_from_slice(chunk);
            if let Some(utterance) = vad.push_frame(&frame).unwrap() {
                chunks.push(utterance);
            }
        }
        if let Some(utterance) = vad.flush() {
            chunks.push(utterance);
        }
        let mut stt = Stt::load(&SttConfig::default()).unwrap();
        let filter = crate::lang::TargetFilter::new("English", "Spanish").unwrap();
        let texts = chunks
            .iter()
            .map(|chunk| stt.transcribe(&chunk.samples).unwrap())
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>();
        let spanish = texts.iter().filter(|text| filter.is_target(text)).count();
        assert!(
            spanish >= 3,
            "La grabación no confirma suficiente voz española: {texts:?}"
        );
        let report = serde_json::json!({"spanish_phrases":spanish,"phrases":texts});
        println!("{report}");
        if let Some(path) = std::env::var_os("VOZ_DUB_RESULT") {
            std::fs::write(path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
        }
    }
    #[test]
    #[ignore = "Medición manual con el modelo Parakeet instalado"]
    fn benchmark_local_parakeet() {
        println!("{}", ort::info());
        let samples =
            crate::audio::read_wav_16k(std::path::Path::new("tests/fixtures/reunion-es.wav"))
                .unwrap();
        let mut segmenter =
            crate::vad::Segmenter::new(&crate::config::VadConfig::default()).unwrap();
        let mut phrases = Vec::new();
        for chunk in samples.chunks(crate::types::FRAME_SAMPLES) {
            let mut frame = vec![0.0; crate::types::FRAME_SAMPLES];
            frame[..chunk.len()].copy_from_slice(chunk);
            if let Some(phrase) = segmenter.push_frame(&frame).unwrap() {
                phrases.push(phrase);
            }
        }
        if let Some(phrase) = segmenter.flush() {
            phrases.push(phrase);
        }
        assert_eq!(phrases.len(), 2);
        let mut model = Stt::load(&SttConfig::default()).unwrap();
        let mut measurements = Vec::new();
        for round in 1..=3 {
            for phrase in &phrases {
                let started = std::time::Instant::now();
                let text = model.transcribe(&phrase.samples).unwrap();
                let seconds = started.elapsed().as_secs_f64();
                assert!(
                    text.to_lowercase().contains(if phrase.id == 1 {
                        "despliegue"
                    } else {
                        "informe"
                    }),
                    "{text}"
                );
                println!(
                    "Parakeet ronda {round} #{}: {seconds:.3} s; {text}",
                    phrase.id
                );
                measurements.push(serde_json::json!({"round":round,"phrase":phrase.id,"seconds":seconds,"text":text}));
            }
        }
        if let Some(path) = std::env::var_os("VOZ_BENCH_RESULT") {
            std::fs::write(path, serde_json::to_vec_pretty(&measurements).unwrap()).unwrap();
        }
    }
}
