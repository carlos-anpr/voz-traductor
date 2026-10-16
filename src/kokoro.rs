//! Inferencia local Kokoro v1.0 y fonemización con espeak-ng.
use crate::{config::TtsConfig, models, types::TTS_SAMPLE_RATE};
use anyhow::{Context, Result, anyhow, bail, ensure};
use libloading::Library;
use ort::{
    session::{Session, builder::GraphOptimizationLevel},
    value::Tensor,
};
use std::{
    collections::HashMap,
    ffi::{CStr, CString, c_char, c_int, c_void},
    fs::File,
    io::Read,
    path::Path,
    sync::{Mutex, OnceLock},
};

const MAX_TOKENS: usize = 510;
const STYLE_WIDTH: usize = 256;
const INTRA_THREADS: usize = 8;
const PUNCTUATION: &str = ";:,.!?¡¿—…\"«»“”(){}[]";
type Initialize = unsafe extern "C" fn(c_int, c_int, *const c_char, c_int) -> c_int;
type SetVoice = unsafe extern "C" fn(*const c_char) -> c_int;
type ToPhonemes = unsafe extern "C" fn(*mut *const c_void, c_int, c_int) -> *const c_char;

struct Espeak {
    _library: Library,
    set_voice: SetVoice,
    to_phonemes: ToPhonemes,
}
impl Espeak {
    fn load() -> Result<Self> {
        // SAFETY: las firmas corresponden a speak_lib.h. La biblioteca se conserva
        // durante toda la vida del proceso y sus llamadas están protegidas por mutex.
        unsafe {
            let library = Library::new("libespeak-ng.so.1").context(
                "No se puede cargar libespeak-ng.so.1; instala libespeak-ng1 y espeak-ng-data",
            )?;
            let initialize: Initialize = *library.get(b"espeak_Initialize\0")?;
            let set_voice = *library.get::<SetVoice>(b"espeak_SetVoiceByName\0")?;
            let to_phonemes = *library.get::<ToPhonemes>(b"espeak_TextToPhonemes\0")?;
            ensure!(
                initialize(2, 0, std::ptr::null(), 0) > 0,
                "No se puede inicializar espeak-ng"
            );
            Ok(Self {
                _library: library,
                set_voice,
                to_phonemes,
            })
        }
    }
    fn fragment(&self, text: &str) -> Result<String> {
        if text.trim().is_empty() {
            return Ok(text.to_owned());
        }
        let input = CString::new(text.trim()).context("El texto contiene un carácter nulo")?;
        let mut pointer = input.as_ptr().cast::<c_void>();
        let mut parts = Vec::new();
        while !pointer.is_null() {
            // SAFETY: input sigue vivo y terminado en NUL. La API avanza pointer;
            // copiamos su resultado estático antes de la siguiente llamada.
            let part = unsafe { (self.to_phonemes)(&mut pointer, 1, (i32::from(b'_') << 8) | 2) };
            if !part.is_null() {
                let value = unsafe { CStr::from_ptr(part) }
                    .to_str()
                    .context("espeak-ng devolvió fonemas UTF-8 no válidos")?;
                parts.push(value.to_owned());
            }
        }
        let raw = parts.join(" ").replace('\n', " ").replace("  ", " ");
        let mut switched = false;
        let phonemes: String = raw
            .chars()
            .filter(|&c| {
                if c == '(' {
                    switched = true;
                    return false;
                }
                if c == ')' {
                    switched = false;
                    return false;
                }
                !switched && c != '_'
            })
            .collect();
        let leading: String = text.chars().take_while(|c| c.is_whitespace()).collect();
        let trailing: String = text
            .chars()
            .rev()
            .take_while(|c| c.is_whitespace())
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        Ok(format!("{leading}{}{trailing}", phonemes.trim()))
    }
}

fn vocabulary() -> &'static HashMap<char, i64> {
    static VOCAB: OnceLock<HashMap<char, i64>> = OnceLock::new();
    VOCAB.get_or_init(|| {
        let config: serde_json::Value =
            serde_json::from_str(include_str!("../assets/kokoro-config.json"))
                .expect("vocabulario Kokoro embebido válido");
        config["vocab"]
            .as_object()
            .expect("vocabulario Kokoro")
            .iter()
            .map(|(symbol, id)| {
                (
                    symbol.chars().next().expect("símbolo"),
                    id.as_i64().expect("id"),
                )
            })
            .collect()
    })
}

fn phonemize(text: &str, language: &str) -> Result<String> {
    static ESPEAK: OnceLock<std::result::Result<Mutex<Espeak>, String>> = OnceLock::new();
    let espeak = ESPEAK
        .get_or_init(|| Espeak::load().map(Mutex::new).map_err(|e| format!("{e:#}")))
        .as_ref()
        .map_err(|e| anyhow!(e.clone()))?
        .lock()
        .map_err(|_| anyhow!("El motor de fonemas no está disponible"))?;
    // espeak denomina `en` a la voz británica; `en-gb` no es un identificador
    // válido para SetVoiceByName en la biblioteca del sistema.
    let voice = CString::new(if language == "en-gb" { "en" } else { language })?;
    ensure!(
        unsafe { (espeak.set_voice)(voice.as_ptr()) } == 0,
        "espeak-ng no dispone del idioma {language}"
    );
    let chars: Vec<_> = text.trim().chars().collect();
    let mut output = String::new();
    let mut start = 0;
    for (index, &character) in chars.iter().enumerate() {
        let decimal = matches!(character, ',' | '.')
            && index > 0
            && chars[index - 1].is_ascii_digit()
            && chars.get(index + 1).is_some_and(char::is_ascii_digit);
        if PUNCTUATION.contains(character) && !decimal {
            output.push_str(&espeak.fragment(&chars[start..index].iter().collect::<String>())?);
            output.push(character);
            start = index + 1;
        }
    }
    output.push_str(&espeak.fragment(&chars[start..].iter().collect::<String>())?);
    Ok(output
        .chars()
        .filter(|c| vocabulary().contains_key(c))
        .collect::<String>()
        .trim()
        .to_owned())
}

fn atoms(phonemes: &str, level: usize, output: &mut Vec<String>) {
    let chars: Vec<_> = phonemes.chars().collect();
    if chars.len() <= MAX_TOKENS {
        if !chars.is_empty() {
            output.push(phonemes.to_owned());
        }
        return;
    }
    for boundary in level..3 {
        let mut pieces = Vec::new();
        let mut start = 0;
        let mut index = 0;
        while index < chars.len() {
            if chars[index].is_whitespace()
                && index > 0
                && match boundary {
                    0 => ".!?…".contains(chars[index - 1]),
                    1 => ",;:".contains(chars[index - 1]),
                    _ => true,
                }
            {
                pieces.push(chars[start..index].iter().collect::<String>());
                while index < chars.len() && chars[index].is_whitespace() {
                    index += 1;
                }
                start = index;
            } else {
                index += 1;
            }
        }
        if !pieces.is_empty() {
            pieces.push(chars[start..].iter().collect());
            for piece in pieces {
                atoms(piece.trim(), boundary + 1, output);
            }
            return;
        }
    }
    output.extend(chars.chunks(MAX_TOKENS).map(|c| c.iter().collect()));
}
fn pack(lengths: &[usize], limit: usize) -> Vec<std::ops::Range<usize>> {
    let mut result = Vec::new();
    let (mut start, mut size) = (0, 0);
    for (index, &length) in lengths.iter().enumerate() {
        let candidate = if index == start {
            length
        } else {
            size + 1 + length
        };
        if candidate > limit && index > start {
            result.push(start..index);
            start = index;
            size = length;
        } else {
            size = candidate;
        }
    }
    if !lengths.is_empty() {
        result.push(start..lengths.len());
    }
    result
}
fn split_phonemes(phonemes: &str) -> Vec<String> {
    let mut pieces = Vec::new();
    atoms(phonemes.trim(), 0, &mut pieces);
    let lengths: Vec<_> = pieces.iter().map(|p| p.chars().count()).collect();
    let Some(&longest) = lengths.iter().max() else {
        return Vec::new();
    };
    let fewest = pack(&lengths, MAX_TOKENS).len();
    let (mut low, mut high) = (longest, MAX_TOKENS);
    while low < high {
        let middle = (low + high) / 2;
        if pack(&lengths, middle).len() <= fewest {
            high = middle;
        } else {
            low = middle + 1;
        }
    }
    pack(&lengths, low)
        .into_iter()
        .map(|range| pieces[range].join(" "))
        .collect()
}

fn read_style(bytes: &[u8]) -> Result<Vec<f32>> {
    ensure!(
        bytes.len() >= 10 && &bytes[..6] == b"\x93NUMPY",
        "Formato NPY de voz no válido"
    );
    let (offset, header_len) = match bytes[6] {
        1 => (10, usize::from(u16::from_le_bytes([bytes[8], bytes[9]]))),
        2 | 3 if bytes.len() >= 12 => (12, u32::from_le_bytes(bytes[8..12].try_into()?) as usize),
        _ => bail!("Versión NPY de voz no compatible"),
    };
    let end = offset + header_len;
    let header = std::str::from_utf8(bytes.get(offset..end).context("Cabecera NPY incompleta")?)?;
    ensure!(
        header.contains("'<f4'") && header.contains("'fortran_order': False"),
        "La voz debe contener floats little-endian en orden C"
    );
    let shape = header
        .split("'shape':")
        .nth(1)
        .and_then(|s| s.split_once('('))
        .and_then(|(_, s)| s.split_once(')'))
        .context("Forma NPY no válida")?
        .0;
    let shape: Vec<usize> = shape
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::parse)
        .collect::<std::result::Result<_, _>>()?;
    ensure!(
        shape == [MAX_TOKENS, 1, STYLE_WIDTH],
        "Forma de voz Kokoro no compatible: {shape:?}"
    );
    let payload = bytes.get(end..).context("Voz NPY incompleta")?;
    ensure!(
        payload.len() == MAX_TOKENS * STYLE_WIDTH * 4,
        "Tamaño de voz Kokoro no válido"
    );
    Ok(payload
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes(chunk.try_into().expect("float de cuatro bytes")))
        .collect())
}
fn load_voice(path: &Path, voice: &str) -> Result<Vec<f32>> {
    let mut archive = zip::ZipArchive::new(File::open(path)?)?;
    let mut entry = archive.by_name(&format!("{voice}.npy")).with_context(|| {
        format!("No existe la voz Kokoro «{voice}»; consulta voces --motor kokoro")
    })?;
    ensure!(
        entry.size() <= (MAX_TOKENS * STYLE_WIDTH * 4 + 4096) as u64,
        "Fichero de voz Kokoro demasiado grande"
    );
    let mut bytes = Vec::new();
    entry.read_to_end(&mut bytes)?;
    read_style(&bytes)
}
pub fn list_voices(dir: &Path) -> Result<Vec<String>> {
    let (_, voices) = models::ensure_kokoro_files(dir)?;
    let archive = zip::ZipArchive::new(File::open(voices)?)?;
    let mut names = Vec::new();
    for name in archive.file_names() {
        let name = name?;
        if let Some(name) = name.strip_suffix(".npy") {
            names.push(name.to_owned());
        }
    }
    names.sort();
    Ok(names)
}

// Equivalente al recorte RMS de la referencia: ventanas centradas de 2048,
// salto 512 y umbral de 60 dB respecto al máximo (suelo de 1e-5).
fn trim_audio(audio: &[f32]) -> &[f32] {
    if audio.is_empty() {
        return audio;
    }
    let mut powers = Vec::with_capacity(audio.len() + 1);
    powers.push(0.0_f64);
    for &sample in audio {
        powers.push(powers.last().copied().unwrap_or(0.0) + f64::from(sample).powi(2));
    }
    let rms_power: Vec<_> = (0..=audio.len() / 512)
        .map(|frame| {
            let center = frame * 512;
            (powers[(center + 1024).min(audio.len())] - powers[center.saturating_sub(1024)])
                / 2048.0
        })
        .collect();
    let reference = rms_power.iter().copied().fold(1e-10_f64, f64::max);
    let mut non_silent = rms_power
        .iter()
        .enumerate()
        .filter(|(_, p)| p.max(1e-10) > reference * 1e-6)
        .map(|(i, _)| i);
    let Some(first) = non_silent.next() else {
        return &audio[..0];
    };
    let last = non_silent.next_back().unwrap_or(first);
    &audio[first * 512..((last + 1) * 512).min(audio.len())]
}
fn language(voice: &str) -> Result<&'static str> {
    match voice.chars().next() {
        Some('a') => Ok("en-us"),
        Some('b') => Ok("en-gb"),
        Some('e') => Ok("es"),
        Some('f') => Ok("fr-fr"),
        Some('h') => Ok("hi"),
        Some('i') => Ok("it"),
        Some('j') => Ok("ja"),
        Some('p') => Ok("pt-br"),
        Some('z') => Ok("cmn"),
        _ => bail!("No se reconoce el idioma de la voz Kokoro «{voice}»"),
    }
}

pub struct Kokoro {
    session: Session,
    style: Vec<f32>,
    speed: f32,
    language: &'static str,
}
impl Kokoro {
    pub fn load(cfg: &TtsConfig) -> Result<Self> {
        ensure!(
            cfg.speed.is_finite() && (0.5..=2.0).contains(&cfg.speed),
            "La velocidad debe estar entre 0,5 y 2"
        );
        let (model, voices) = models::ensure_kokoro_files(&cfg.kokoro_dir)?;
        Self::load_files(cfg, &model, &voices)
    }
    fn load_files(cfg: &TtsConfig, model: &Path, voices: &Path) -> Result<Self> {
        let style = load_voice(voices, &cfg.voice)?;
        let threads =
            INTRA_THREADS.min(std::thread::available_parallelism().map_or(1, |n| n.get()));
        #[cfg(test)]
        let threads = std::env::var("VOZ_BENCH_THREADS")
            .ok()
            .map(|s| s.parse::<usize>().expect("Número de hilos del benchmark"))
            .unwrap_or(threads);
        let session = Session::builder()?
            .with_intra_threads(threads)
            .map_err(ort::Error::<()>::from)?
            .with_optimization_level(GraphOptimizationLevel::All)
            .map_err(ort::Error::<()>::from)?
            .commit_from_file(model)
            .with_context(|| format!("No se puede cargar Kokoro desde {}", model.display()))?;
        let mut result = Self {
            session,
            style,
            speed: cfg.speed,
            language: language(&cfg.voice)?,
        };
        result
            .synthesize("Hello.")
            .context("No se pudo calentar Kokoro")?;
        Ok(result)
    }
    pub fn synthesize(&mut self, text: &str) -> Result<Vec<f32>> {
        let phonemes = phonemize(text, self.language)?;
        let normalized = phonemes.split_whitespace().collect::<Vec<_>>().join(" ");
        let batches = split_phonemes(&normalized);
        ensure!(
            !batches.is_empty(),
            "El texto no contiene fonemas para sintetizar"
        );
        let mut result = Vec::new();
        for (index, batch) in batches.iter().enumerate() {
            let tokens: Vec<i64> = batch
                .chars()
                .filter_map(|c| vocabulary().get(&c).copied())
                .collect();
            ensure!(
                !tokens.is_empty() && tokens.len() <= MAX_TOKENS,
                "Número de fonemas Kokoro no válido"
            );
            let row = tokens.len() - 1;
            let style = self.style[row * STYLE_WIDTH..(row + 1) * STYLE_WIDTH].to_vec();
            let mut padded = Vec::with_capacity(tokens.len() + 2);
            padded.push(0);
            padded.extend(tokens);
            padded.push(0);
            let token_tensor = Tensor::from_array(([1, padded.len()], padded))?;
            let style_tensor = Tensor::from_array(([1, STYLE_WIDTH], style))?;
            let speed_tensor = Tensor::from_array(([1], vec![self.speed]))?;
            let output = self.session.run(ort::inputs!["tokens" => token_tensor, "style" => style_tensor, "speed" => speed_tensor])?;
            let (_, audio) = output[0].try_extract_tensor::<f32>()?;
            ensure!(
                !audio.is_empty() && audio.iter().all(|s| s.is_finite()),
                "Kokoro devolvió audio vacío o no válido"
            );
            result.extend_from_slice(trim_audio(audio));
            if index + 1 < batches.len() {
                let seconds = match batch.chars().last() {
                    Some(c) if ".!?…".contains(c) => 0.25,
                    Some(c) if ",;:".contains(c) => 0.1,
                    _ => 0.0,
                };
                result.resize(
                    result.len() + (seconds * TTS_SAMPLE_RATE as f32) as usize,
                    0.0,
                );
            }
        }
        ensure!(!result.is_empty(), "Kokoro devolvió audio vacío");
        Ok(result)
    }
    pub fn set_speed(&mut self, speed: f32) -> Result<()> {
        ensure!(
            speed.is_finite() && (0.5..=2.0).contains(&speed),
            "Velocidad Kokoro fuera de rango"
        );
        self.speed = speed;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn spanish_phonemes_and_tokens_match_python_reference() {
        let reference: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/kokoro-phonemes-es.json"))
                .unwrap();
        for row in reference.as_array().unwrap() {
            let actual = phonemize(row["text"].as_str().unwrap(), "es").unwrap();
            assert_eq!(actual, row["phonemes"].as_str().unwrap());
            let tokens: Vec<_> = actual
                .chars()
                .filter_map(|c| vocabulary().get(&c).copied())
                .collect();
            assert_eq!(serde_json::to_value(tokens).unwrap(), row["tokens"]);
        }
    }
    #[test]
    fn phonemes_and_tokens_match_python_reference() {
        let reference: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/kokoro-phonemes.json")).unwrap();
        for row in reference.as_array().unwrap() {
            let actual = phonemize(row["text"].as_str().unwrap(), "en-gb").unwrap();
            assert_eq!(actual, row["phonemes"].as_str().unwrap());
            let tokens: Vec<_> = actual.chars().map(|c| vocabulary()[&c]).collect();
            assert_eq!(serde_json::to_value(tokens).unwrap(), row["tokens"]);
        }
    }
    #[test]
    fn chunks_keep_the_tail_and_balance_without_breaking_unicode() {
        let text = "həlˈəʊ wˈɜːld. ".repeat(100);
        let chunks = split_phonemes(&text);
        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|c| c.chars().count() <= MAX_TOKENS));
        assert_eq!(chunks.join(" "), text.trim());
        let unbroken = "ə".repeat(1021);
        let chunks = split_phonemes(&unbroken);
        assert_eq!(
            chunks.iter().map(|c| c.chars().count()).collect::<Vec<_>>(),
            [510, 510, 1]
        );
        assert_eq!(chunks.concat(), unbroken);
    }
    #[test]
    fn trim_uses_centered_rms_frames_and_preserves_uniform_silence() {
        let mut audio = vec![0.0; 8192];
        audio[4096] = 1.0;
        assert_eq!(trim_audio(&audio).len(), 2048);
        assert_eq!(trim_audio(&[0.0; 100]).len(), 100);
        assert!(trim_audio(&[]).is_empty());
    }
    #[test]
    fn invalid_voice_arrays_are_rejected() {
        assert!(read_style(b"bad").is_err());
        let mut bytes = b"\x93NUMPY\x01\x00\x10\x00bad".to_vec();
        assert!(read_style(&bytes).is_err());
        bytes[6] = 4;
        assert!(read_style(&bytes).is_err());
    }
    #[test]
    #[ignore = "Medición manual con el modelo Kokoro instalado"]
    fn benchmark_local_kokoro() {
        println!("{}", ort::info());
        let cfg = TtsConfig::default();
        let model = std::env::var_os("VOZ_BENCH_MODEL").map(std::path::PathBuf::from);
        let mut engine = if let Some(model) = &model {
            Kokoro::load_files(&cfg, model, &cfg.kokoro_dir.join("voices-v1.0.bin")).unwrap()
        } else {
            Kokoro::load(&cfg).unwrap()
        };
        println!(
            "Modelo: {}",
            model
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "predeterminado".into())
        );
        let mut measurements = Vec::new();
        for round in 1..=3 {
            let reference: serde_json::Value =
                serde_json::from_str(include_str!("../tests/fixtures/kokoro-phonemes.json"))
                    .unwrap();
            for (index, row) in reference.as_array().unwrap().iter().enumerate() {
                let started = std::time::Instant::now();
                let audio = engine.synthesize(row["text"].as_str().unwrap()).unwrap();
                let elapsed = started.elapsed().as_secs_f64();
                measurements.push(serde_json::json!({"round": round, "phrase": index + 1, "seconds": elapsed, "samples": audio.len()}));
                println!(
                    "Kokoro Rust ronda {} #{}: {:.3} s; {} muestras",
                    round,
                    index + 1,
                    elapsed,
                    audio.len()
                );
                assert!(audio.iter().any(|s| s.abs() > 0.01));
                let path = std::env::temp_dir().join(format!("voz-kokoro-rust-{}.wav", index + 1));
                let mut writer = hound::WavWriter::create(
                    path,
                    hound::WavSpec {
                        channels: 1,
                        sample_rate: TTS_SAMPLE_RATE,
                        bits_per_sample: 32,
                        sample_format: hound::SampleFormat::Float,
                    },
                )
                .unwrap();
                for sample in audio {
                    writer.write_sample(sample).unwrap();
                }
                writer.finalize().unwrap();
            }
        }
        if let Some(path) = std::env::var_os("VOZ_BENCH_RESULT") {
            std::fs::write(path, serde_json::to_vec_pretty(&measurements).unwrap()).unwrap();
        }
    }
}
