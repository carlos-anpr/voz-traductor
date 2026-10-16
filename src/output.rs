//! Sesión en disco: una carpeta por ejecución con el audio traducido y la transcripción.

use crate::config::Config;
use crate::types::{Phrase, SAMPLE_RATE, TTS_SAMPLE_RATE, Timings};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

const SILENCE_BETWEEN_SECS: f32 = 0.3;

/// WAV incremental de doblaje; cada flush actualiza la cabecera y hace el audio recuperable.
pub struct DubWriter(hound::WavWriter<BufWriter<File>>);
impl DubWriter {
    pub fn create(path: &Path) -> anyhow::Result<Self> {
        Ok(Self(hound::WavWriter::create(
            path,
            hound::WavSpec {
                channels: 1,
                sample_rate: TTS_SAMPLE_RATE,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            },
        )?))
    }
    pub fn append(&mut self, audio: &[f32]) -> anyhow::Result<()> {
        for sample in audio {
            self.0
                .write_sample((sample.clamp(-1.0, 1.0) * 32767.0) as i16)?;
        }
        self.0.flush()?;
        Ok(())
    }
    pub fn finish(self) -> anyhow::Result<()> {
        self.0.finalize()?;
        Ok(())
    }
}

pub struct Session {
    dir: PathBuf,
    wav: hound::WavWriter<BufWriter<File>>,
    trad: hound::WavWriter<BufWriter<File>>,
    md: File,
    has_original: bool,
    has_translated: bool,
    timings: Vec<Timings>,
    providers: Vec<String>,
}

impl Session {
    /// Crea `<dir>/YYYYMMDD-HHMMSS/`. Crea la carpeta base si no existe y guarda
    /// la ruta absoluta. `cfg` sirve para la cabecera de la transcripción (modelo, voz...).
    pub fn create(dir: &Path, cfg: &Config) -> anyhow::Result<Self> {
        std::fs::create_dir_all(dir)
            .map_err(|e| anyhow::anyhow!("no se pudo crear {}: {}", dir.display(), e))?;
        let dir = std::path::absolute(dir).map_err(|e| anyhow::anyhow!("ruta no válida: {e}"))?;
        let base_ts = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
        let mut session_dir = dir.join(&base_ts);
        let mut n = 1;
        while session_dir.exists() {
            n += 1;
            session_dir = dir.join(format!("{base_ts}-{n}"));
        }
        std::fs::create_dir_all(session_dir.join("frases"))
            .map_err(|e| anyhow::anyhow!("no se pudo crear {}: {}", session_dir.display(), e))?;
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: SAMPLE_RATE,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let wav = hound::WavWriter::create(session_dir.join("original.wav"), spec)
            .map_err(|e| anyhow::anyhow!("no se pudo crear original.wav: {e}"))?;
        let trad_spec = hound::WavSpec {
            channels: 1,
            sample_rate: TTS_SAMPLE_RATE,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let trad = hound::WavWriter::create(session_dir.join("traduccion.wav"), trad_spec)
            .map_err(|e| anyhow::anyhow!("no se pudo crear traduccion.wav: {e}"))?;
        let started = chrono::Local::now();
        let header = format!(
            "# Traducción de voz — {}\n\n- Fecha: {}\n- Modelo de traducción: {}\n- Voz: {}\n\n\
             | # | Español | English | Proveedor | STT | Trad. | Voz | Total |\n\
             |---|---|---|---|---|---|---|---|\n",
            started.format("%Y-%m-%d %H:%M:%S"),
            started.format("%Y-%m-%d %H:%M:%S"),
            cfg.translate.describe(),
            cfg.tts.voice,
        );
        let mut md = File::create(session_dir.join("transcripcion.md"))
            .map_err(|e| anyhow::anyhow!("no se pudo crear transcripcion.md: {e}"))?;
        md.write_all(header.as_bytes())
            .map_err(|e| anyhow::anyhow!("no se pudo escribir la cabecera: {e}"))?;
        md.flush()
            .map_err(|e| anyhow::anyhow!("no se pudo volcar transcripcion.md: {e}"))?;
        Ok(Self {
            dir: session_dir,
            wav,
            trad,
            md,
            has_original: false,
            has_translated: false,
            timings: Vec::new(),
            providers: Vec::new(),
        })
    }

    /// Guarda la frase al momento (si el programa muere, lo escrito sigue sirviendo).
    pub fn write(&mut self, phrase: &Phrase) -> anyhow::Result<()> {
        let es_name = format!("{:03}-es.wav", phrase.id);
        let en_name = format!("{:03}-en.wav", phrase.id);
        write_wav_mono16(
            &self.dir.join("frases").join(es_name),
            &phrase.original,
            SAMPLE_RATE,
        )?;
        write_wav_mono16(
            &self.dir.join("frases").join(en_name),
            &phrase.audio,
            TTS_SAMPLE_RATE,
        )?;

        if self.has_translated && !phrase.audio.is_empty() {
            let gap = (SILENCE_BETWEEN_SECS * TTS_SAMPLE_RATE as f32) as usize;
            for _ in 0..gap {
                self.trad
                    .write_sample(0i16)
                    .map_err(|e| anyhow::anyhow!("no se pudo escribir traduccion.wav: {e}"))?;
            }
        }
        for s in &phrase.audio {
            let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
            self.trad
                .write_sample(v)
                .map_err(|e| anyhow::anyhow!("no se pudo escribir traduccion.wav: {e}"))?;
        }
        if !phrase.audio.is_empty() {
            self.has_translated = true;
        }
        self.trad
            .flush()
            .map_err(|e| anyhow::anyhow!("no se pudo volcar traduccion.wav: {e}"))?;

        if self.has_original && !phrase.original.is_empty() {
            let gap = (SILENCE_BETWEEN_SECS * SAMPLE_RATE as f32) as usize;
            for _ in 0..gap {
                self.wav
                    .write_sample(0i16)
                    .map_err(|e| anyhow::anyhow!("no se pudo escribir original.wav: {e}"))?;
            }
        }
        for s in &phrase.original {
            let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
            self.wav
                .write_sample(v)
                .map_err(|e| anyhow::anyhow!("no se pudo escribir original.wav: {e}"))?;
        }
        if !phrase.original.is_empty() {
            self.has_original = true;
        }
        self.wav
            .flush()
            .map_err(|e| anyhow::anyhow!("no se pudo volcar original.wav: {e}"))?;

        let row = format!(
            "| {} | {} | {} | {} | {} | {} | {} | {} |\n",
            phrase.id,
            esc_cell(&phrase.source_text),
            esc_cell(&phrase.target_text),
            esc_cell(&phrase.provider),
            fmt_dur(phrase.timings.stt),
            fmt_dur(phrase.timings.translate),
            fmt_dur(phrase.timings.tts),
            fmt_dur(phrase.timings.total),
        );
        self.md
            .write_all(row.as_bytes())
            .map_err(|e| anyhow::anyhow!("no se pudo escribir la frase {}: {e}", phrase.id))?;
        self.md
            .flush()
            .map_err(|e| anyhow::anyhow!("no se pudo volcar transcripcion.md: {e}"))?;

        self.timings.push(phrase.timings.clone());
        self.providers.push(phrase.provider.clone());
        Ok(())
    }

    /// Cierra la transcripción con el resumen (nº frases, latencias media/p50/máx por etapa).
    pub fn finish(mut self) -> anyhow::Result<PathBuf> {
        let n = self.timings.len();
        writeln!(self.md, "\n## Resumen\n")
            .map_err(|e| anyhow::anyhow!("no se pudo cerrar la transcripción: {e}"))?;
        writeln!(self.md, "- Frases: {n}")
            .map_err(|e| anyhow::anyhow!("no se pudo cerrar la transcripción: {e}"))?;
        if n > 0 {
            let mut counts: Vec<(&str, usize)> = Vec::new();
            for p in &self.providers {
                match counts.iter_mut().find(|(name, _)| *name == p) {
                    Some(entry) => entry.1 += 1,
                    None => counts.push((p.as_str(), 1)),
                }
            }
            let by_provider = counts
                .iter()
                .map(|(name, c)| format!("{name} ×{c}"))
                .collect::<Vec<_>>()
                .join(", ");
            writeln!(self.md, "- Por proveedor: {by_provider}")
                .map_err(|e| anyhow::anyhow!("no se pudo cerrar la transcripción: {e}"))?;
            let stt: Vec<f64> = self.timings.iter().map(|t| t.stt.as_secs_f64()).collect();
            let tra: Vec<f64> = self
                .timings
                .iter()
                .map(|t| t.translate.as_secs_f64())
                .collect();
            let voz: Vec<f64> = self.timings.iter().map(|t| t.tts.as_secs_f64()).collect();
            let tot: Vec<f64> = self.timings.iter().map(|t| t.total.as_secs_f64()).collect();
            writeln!(
                self.md,
                "- STT: media {} · p50 {} · máx {}",
                fmt_secs(mean(&stt)),
                fmt_secs(percentile(&stt, 50.0)),
                fmt_secs(max(&stt))
            )
            .map_err(|e| anyhow::anyhow!("no se pudo cerrar la transcripción: {e}"))?;
            writeln!(
                self.md,
                "- Traducción: media {} · p50 {} · máx {}",
                fmt_secs(mean(&tra)),
                fmt_secs(percentile(&tra, 50.0)),
                fmt_secs(max(&tra))
            )
            .map_err(|e| anyhow::anyhow!("no se pudo cerrar la transcripción: {e}"))?;
            writeln!(
                self.md,
                "- Voz: media {} · p50 {} · máx {}",
                fmt_secs(mean(&voz)),
                fmt_secs(percentile(&voz, 50.0)),
                fmt_secs(max(&voz))
            )
            .map_err(|e| anyhow::anyhow!("no se pudo cerrar la transcripción: {e}"))?;
            writeln!(
                self.md,
                "- Total: media {} · p50 {} · máx {}",
                fmt_secs(mean(&tot)),
                fmt_secs(percentile(&tot, 50.0)),
                fmt_secs(max(&tot))
            )
            .map_err(|e| anyhow::anyhow!("no se pudo cerrar la transcripción: {e}"))?;
        }
        self.md
            .flush()
            .map_err(|e| anyhow::anyhow!("no se pudo volcar transcripcion.md: {e}"))?;
        self.wav
            .finalize()
            .map_err(|e| anyhow::anyhow!("no se pudo cerrar original.wav: {e}"))?;
        self.trad
            .finalize()
            .map_err(|e| anyhow::anyhow!("no se pudo cerrar traduccion.wav: {e}"))?;
        Ok(self.dir)
    }

    /// Elimina únicamente una sesión propia que todavía no contiene frases.
    pub fn discard(self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.timings.is_empty(),
            "No se puede descartar una sesión con frases"
        );
        let dir = self.dir.clone();
        drop(self);
        std::fs::remove_dir_all(&dir)
            .map_err(|error| anyhow::anyhow!("No se pudo descartar {}: {error}", dir.display()))
    }

    pub fn path(&self) -> &Path {
        &self.dir
    }
}

fn write_wav_mono16(path: &Path, samples: &[f32], sample_rate: u32) -> anyhow::Result<()> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec)
        .map_err(|e| anyhow::anyhow!("no se pudo crear {}: {e}", path.display()))?;
    for s in samples {
        let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
        writer
            .write_sample(v)
            .map_err(|e| anyhow::anyhow!("no se pudo escribir {}: {e}", path.display()))?;
    }
    writer
        .finalize()
        .map_err(|e| anyhow::anyhow!("no se pudo cerrar {}: {e}", path.display()))?;
    Ok(())
}

fn fmt_dur(d: std::time::Duration) -> String {
    fmt_secs(d.as_secs_f64())
}

fn fmt_secs(secs: f64) -> String {
    format!("{:.2}", secs).replace('.', ",") + " s"
}

pub(crate) fn esc_cell(s: &str) -> String {
    s.replace('|', "\\|")
        .replace("\r\n", " ")
        .replace(['\r', '\n'], " ")
        .trim()
        .to_owned()
}

fn mean(v: &[f64]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.iter().sum::<f64>() / v.len() as f64
}

fn max(v: &[f64]) -> f64 {
    v.iter().cloned().fold(0.0, f64::max)
}

fn percentile(v: &[f64], p: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    let mut sorted = v.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let rank = (p / 100.0 * sorted.len() as f64).ceil() as usize;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn tmp_root() -> PathBuf {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let p = std::env::temp_dir().join(format!(
            "voz-traductor-test-{}-{}-{}",
            std::process::id(),
            chrono::Local::now().timestamp_millis(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn phrase(id: u32, es: &str, en: &str) -> Phrase {
        Phrase {
            id,
            original: vec![0.0; 1600],
            source_text: es.into(),
            target_text: en.into(),
            provider: "groq".into(),
            audio: vec![0.0; 2400],
            timings: Timings {
                stt: Duration::from_millis(310),
                translate: Duration::from_millis(2050),
                tts: Duration::from_millis(820),
                total: Duration::from_millis(3200),
            },
        }
    }

    fn test_cfg() -> Config {
        let mut cfg = Config::default();
        cfg.translate.providers = vec![crate::config::TranslateProvider {
            name: "modelo-prueba".into(),
            model: "modelo-prueba".into(),
            ..Default::default()
        }];
        cfg.tts.voice = "voz-prueba".into();
        cfg
    }

    #[test]
    fn sesion_guarda_ficheros_y_resumen() {
        let root = tmp_root();
        let cfg = test_cfg();
        let mut s = Session::create(&root, &cfg).unwrap();
        let dir = s.path().to_path_buf();
        let mut p1 = phrase(1, "hola | mundo", "hello world");
        p1.original = vec![0.1; 1600];
        p1.audio = vec![0.2; 2400];
        s.write(&p1).unwrap();
        s.write(&phrase(2, "segunda", "second")).unwrap();
        let out = s.finish().unwrap();
        assert_eq!(out, dir);
        assert!(dir.join("traduccion.wav").exists());
        assert!(!dir.join("traduccion.mp3").exists());
        assert!(dir.join("frases/001-es.wav").exists());
        assert!(dir.join("frases/001-en.wav").exists());
        assert!(!dir.join("frases/001-en.mp3").exists());
        assert!(dir.join("frases/002-es.wav").exists());
        assert!(dir.join("frases/002-en.wav").exists());
        let md = std::fs::read_to_string(dir.join("transcripcion.md")).unwrap();
        assert!(md.contains("modelo-prueba (modelo-prueba)"));
        assert!(md.contains("voz-prueba"));
        assert!(md.contains("hola \\| mundo"));
        assert!(md.contains("| groq |"));
        assert!(md.contains("0,31 s"));
        assert!(md.contains("- Frases: 2"));
        assert!(md.contains("- Por proveedor: groq ×2"));
        let mut reader = hound::WavReader::open(dir.join("original.wav")).unwrap();
        assert_eq!(reader.spec().sample_rate, SAMPLE_RATE);
        assert_eq!(reader.samples::<i16>().count(), 1600 + 4800 + 1600);
        let mut trad = hound::WavReader::open(dir.join("traduccion.wav")).unwrap();
        assert_eq!(trad.spec().sample_rate, TTS_SAMPLE_RATE);
        assert_eq!(trad.samples::<i16>().count(), 2400 + 7200 + 2400);
        let en = hound::WavReader::open(dir.join("frases/001-en.wav")).unwrap();
        assert_eq!(en.spec().sample_rate, TTS_SAMPLE_RATE);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn wav_valido_antes_de_finish() {
        let root = tmp_root();
        let cfg = Config::default();
        let mut s = Session::create(&root, &cfg).unwrap();
        let dir = s.path().to_path_buf();
        s.write(&phrase(1, "una", "one")).unwrap();
        drop(s);
        let reader = hound::WavReader::open(dir.join("original.wav"));
        assert!(reader.is_ok());
        assert_eq!(reader.unwrap().samples::<i16>().count(), 1600);
        let trad = hound::WavReader::open(dir.join("traduccion.wav"));
        assert!(trad.is_ok());
        assert_eq!(trad.unwrap().samples::<i16>().count(), 2400);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn discard_empty_session_keeps_parent_and_sibling() {
        let root = tmp_root().join("discard");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("keep.txt"), "conservar").unwrap();
        let session = Session::create(&root, &Config::default()).unwrap();
        let path = session.path().to_path_buf();
        session.discard().unwrap();
        assert!(!path.exists());
        assert_eq!(
            std::fs::read_to_string(root.join("keep.txt")).unwrap(),
            "conservar"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn discard_refuses_session_with_phrases() {
        let root = tmp_root().join("nonempty");
        let mut session = Session::create(&root, &Config::default()).unwrap();
        let path = session.path().to_path_buf();
        session.write(&phrase(1, "hola", "hello")).unwrap();
        assert!(session.discard().is_err());
        assert!(path.join("frases/001-es.wav").is_file());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn escape_celda() {
        assert_eq!(esc_cell("a|b\nc"), "a\\|b c");
    }
}
