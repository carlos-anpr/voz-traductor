//! Captura y decodificación WAV a tramas mono de 16 kHz.
use crate::types::{FRAME_SAMPLES, SAMPLE_RATE};
use anyhow::{Context, Result, bail, ensure};
use cpal::{
    FromSample, Sample, SizedSample,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use rubato::{Fft, FixedSync, Resampler, audioadapter_buffers::direct::InterleavedSlice};
use std::{
    collections::VecDeque,
    io,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, SyncSender},
    },
    time::Duration,
};

#[derive(Debug, Clone, PartialEq, Eq)]
struct PulseSource {
    name: String,
    description: String,
    default: bool,
}
impl PulseSource {
    fn label(&self) -> String {
        format!("{} [{}]", self.description, self.name)
    }
}

fn parse_pulse_sources(bytes: &[u8], default: &str) -> Result<Vec<PulseSource>> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).context("Respuesta JSON de pactl no válida")?;
    let sources = value
        .as_array()
        .context("pactl no devolvió una lista de fuentes")?;
    let mut result = Vec::new();
    for source in sources {
        let name = source["name"].as_str().context("Fuente Pulse sin nombre")?;
        let monitor_of_sink = &source["monitor_of_sink"];
        let is_monitor = name.ends_with(".monitor")
            || source["properties"]["device.class"].as_str() == Some("monitor")
            || source["monitor_source"]
                .as_str()
                .is_some_and(|s| !s.is_empty())
            || (!monitor_of_sink.is_null() && monitor_of_sink.as_u64() != Some(u32::MAX as u64));
        if is_monitor || result.iter().any(|s: &PulseSource| s.name == name) {
            continue;
        }
        result.push(PulseSource {
            name: name.to_owned(),
            description: source["description"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or(name)
                .to_owned(),
            default: name == default.trim(),
        });
    }
    result.sort_by(|a, b| a.description.cmp(&b.description).then(a.name.cmp(&b.name)));
    Ok(result)
}

fn pulse_sources() -> Result<Option<Vec<PulseSource>>> {
    let output = match std::process::Command::new("pactl")
        .args(["-f", "json", "list", "sources"])
        .output()
    {
        Ok(output) => output,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("No se puede ejecutar pactl"),
    };
    ensure!(
        output.status.success(),
        "No se pueden consultar las fuentes Pulse: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let default = std::process::Command::new("pactl")
        .arg("get-default-source")
        .output()
        .context("No se puede consultar la fuente predeterminada")?;
    ensure!(
        default.status.success(),
        "No se puede consultar la fuente Pulse predeterminada"
    );
    Ok(Some(parse_pulse_sources(
        &output.stdout,
        &String::from_utf8_lossy(&default.stdout),
    )?))
}

fn select_pulse_source(sources: &[PulseSource], query: Option<&str>) -> Result<PulseSource> {
    let Some(query) = query else {
        return sources
            .iter()
            .find(|s| s.default)
            .cloned()
            .context("No hay una fuente de micrófono predeterminada en Pulse");
    };
    let query = query.to_lowercase();
    if let Some(source) = sources.iter().find(|s| s.name.to_lowercase() == query) {
        return Ok(source.clone());
    }
    let matches: Vec<_> = sources
        .iter()
        .filter(|s| {
            s.name.to_lowercase().contains(&query) || s.description.to_lowercase().contains(&query)
        })
        .collect();
    match matches.as_slice() {
        [source] => Ok((*source).clone()),
        [] => bail!("No se encuentra el micrófono «{query}»"),
        _ => bail!(
            "El nombre «{query}» coincide con varios micrófonos; usa su nombre completo: {}",
            matches
                .iter()
                .map(|s| s.label())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Reinicia antes de arrancar las etapas para que libpulse herede PULSE_SOURCE,
/// sin modificar el entorno de un proceso multihilo mediante set_var.
pub fn prepare_microphone(name: Option<&str>) -> Result<()> {
    let Some(sources) = pulse_sources()? else {
        return Ok(());
    };
    let source = select_pulse_source(&sources, name)?;
    if std::env::var_os("PULSE_SOURCE").as_deref() == Some(std::ffi::OsStr::new(&source.name)) {
        return Ok(());
    }
    let mut command = std::process::Command::new(std::env::current_exe()?);
    command
        .args(std::env::args_os().skip(1))
        .env("PULSE_SOURCE", &source.name);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        Err(command.exec()).context("No se puede preparar la fuente Pulse")
    }
    #[cfg(not(unix))]
    {
        bail!("La selección de fuentes Pulse requiere un sistema Unix")
    }
}

fn fallback_device_label(name: &str, id: &str, default: bool) -> Option<String> {
    if !default
        && !["hw:", "plughw:"]
            .iter()
            .any(|prefix| id.starts_with(prefix))
    {
        return None;
    }
    let label = name.trim().trim_end_matches(',').trim();
    (!label.is_empty()).then(|| label.to_owned())
}

fn cpal_input_devices(host: &cpal::Host) -> Result<Vec<(cpal::Device, String, bool)>> {
    let default_id = host.default_input_device().and_then(|d| d.id().ok());
    let mut result: Vec<(cpal::Device, String, bool)> = Vec::new();
    for device in host.input_devices()? {
        let id = device.id()?;
        let default = Some(&id) == default_id.as_ref();
        let Some(label) = fallback_device_label(&device.to_string(), id.id(), default) else {
            continue;
        };
        if let Some(existing) = result
            .iter_mut()
            .find(|(_, name, _)| name.eq_ignore_ascii_case(&label))
        {
            if default {
                *existing = (device, label, true);
            }
        } else {
            result.push((device, label, default));
        }
    }
    Ok(result)
}

pub fn list_input_devices() -> Result<Vec<(String, bool)>> {
    if let Some(sources) = pulse_sources()? {
        return Ok(sources
            .into_iter()
            .map(|s| (s.label(), s.default))
            .collect());
    }
    Ok(cpal_input_devices(&cpal::default_host())?
        .into_iter()
        .map(|(_, name, default)| (name, default))
        .collect())
}

fn mono(samples: &[f32], channels: usize) -> Vec<f32> {
    samples
        .chunks_exact(channels)
        .map(|frame| frame.iter().sum::<f32>() / channels as f32)
        .collect()
}

pub fn read_wav_16k(path: &Path) -> Result<Vec<f32>> {
    let mut reader = hound::WavReader::open(path)
        .with_context(|| format!("No se puede abrir {}", path.display()))?;
    let spec = reader.spec();
    ensure!(
        spec.channels > 0 && spec.sample_rate > 0,
        "El WAV tiene canales o frecuencia no válidos"
    );
    let samples = match spec.sample_format {
        hound::SampleFormat::Float => reader
            .samples::<f32>()
            .collect::<std::result::Result<Vec<_>, _>>()?,
        hound::SampleFormat::Int => {
            let divisor = 2_f32.powi(i32::from(spec.bits_per_sample) - 1);
            reader
                .samples::<i32>()
                .map(|s| s.map(|s| s as f32 / divisor))
                .collect::<std::result::Result<Vec<_>, _>>()?
        }
    };
    let samples = mono(&samples, usize::from(spec.channels));
    if samples.is_empty() || spec.sample_rate == SAMPLE_RATE {
        return Ok(samples);
    }
    let mut resampler = Fft::<f32>::new(
        spec.sample_rate as usize,
        SAMPLE_RATE as usize,
        1024,
        1,
        FixedSync::Input,
    )?;
    let input = InterleavedSlice::new(&samples, 1, samples.len())?;
    Ok(resampler
        .process_all(&input, samples.len(), None)?
        .take_data())
}

pub fn send_frames(samples: &[f32], tx: &SyncSender<Vec<f32>>, stop: &AtomicBool) {
    for chunk in samples.chunks(FRAME_SAMPLES) {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let mut frame = vec![0.0; FRAME_SAMPLES];
        frame[..chunk.len()].copy_from_slice(chunk);
        if tx.send(frame).is_err() {
            break;
        }
    }
}

const BLOCK_SAMPLES: usize = 8192;
type Block = (Box<[f32; BLOCK_SAMPLES]>, usize);

fn build_stream<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    tx: SyncSender<Block>,
    pool: mpsc::Receiver<Box<[f32; BLOCK_SAMPLES]>>,
    lost: Arc<AtomicUsize>,
    errors: SyncSender<cpal::Error>,
) -> Result<cpal::Stream>
where
    T: SizedSample + Sample,
    f32: FromSample<T>,
{
    Ok(device.build_input_stream(
        config,
        move |data: &[T], _| {
            for input in data.chunks(BLOCK_SAMPLES) {
                let Ok(mut buffer) = pool.try_recv() else {
                    lost.fetch_add(1, Ordering::Relaxed);
                    continue;
                };
                for (dst, &src) in buffer.iter_mut().zip(input) {
                    *dst = f32::from_sample(src);
                }
                if tx.try_send((buffer, input.len())).is_err() {
                    lost.fetch_add(1, Ordering::Relaxed);
                }
            }
        },
        move |error| {
            let _ = errors.try_send(error);
        },
        None,
    )?)
}

struct StreamingResampler {
    resampler: Fft<f32>,
    input: VecDeque<f32>,
    output: VecDeque<f32>,
    skip: usize,
    received: usize,
    produced: usize,
    rate: usize,
}
impl StreamingResampler {
    fn new(rate: usize) -> Result<Self> {
        let resampler = Fft::new(rate, SAMPLE_RATE as usize, 1024, 1, FixedSync::Input)?;
        let skip = resampler.output_delay();
        Ok(Self {
            resampler,
            input: VecDeque::new(),
            output: VecDeque::new(),
            skip,
            received: 0,
            produced: 0,
            rate,
        })
    }
    fn push(&mut self, samples: &[f32], tx: &SyncSender<Vec<f32>>) -> Result<bool> {
        self.received += samples.len();
        self.input.extend(samples);
        self.process(tx, false)
    }
    fn process(&mut self, tx: &SyncSender<Vec<f32>>, finish: bool) -> Result<bool> {
        let target = self.received * SAMPLE_RATE as usize / self.rate;
        loop {
            let count = self.resampler.input_frames_next();
            if self.input.len() < count {
                if !finish || self.produced >= target {
                    break;
                }
                self.input.resize(count, 0.0);
            }
            let samples: Vec<_> = self.input.drain(..count).collect();
            let input = InterleavedSlice::new(&samples, 1, count)?;
            let output = self.resampler.process(&input, None)?.take_data();
            for sample in output {
                if self.skip > 0 {
                    self.skip -= 1;
                    continue;
                }
                if finish && self.produced >= target {
                    break;
                }
                self.output.push_back(sample);
                self.produced += 1;
            }
            while self.output.len() >= FRAME_SAMPLES {
                if tx
                    .send(self.output.drain(..FRAME_SAMPLES).collect())
                    .is_err()
                {
                    return Ok(false);
                }
            }
            if finish && self.produced >= target {
                break;
            }
        }
        if finish && !self.output.is_empty() {
            self.output.resize(FRAME_SAMPLES, 0.0);
            if tx.send(self.output.drain(..).collect()).is_err() {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

/// Debe ejecutarse en el hilo de la fuente: el Stream no sale de este hilo.
pub fn capture_microphone(
    name: Option<&str>,
    tx: SyncSender<Vec<f32>>,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    let pulse = pulse_sources()?
        .map(|sources| select_pulse_source(&sources, name))
        .transpose()?;
    if let Some(source) = &pulse {
        ensure!(
            std::env::var_os("PULSE_SOURCE").as_deref() == Some(std::ffi::OsStr::new(&source.name)),
            "La fuente Pulse debe prepararse antes de iniciar la captura"
        );
    }
    let host = cpal::default_host();
    let (device, label) = if let Some(source) = pulse {
        (
            host.default_input_device()
                .context("No hay micrófono predeterminado")?,
            source.label(),
        )
    } else if let Some(name) = name {
        let (device, label, _) = cpal_input_devices(&host)?
            .into_iter()
            .find(|(_, label, _)| label.to_lowercase().contains(&name.to_lowercase()))
            .with_context(|| format!("No se encuentra el micrófono «{name}»"))?;
        (device, label)
    } else {
        let device = host
            .default_input_device()
            .context("No hay micrófono predeterminado")?;
        let label = device.to_string();
        (device, label)
    };
    println!("Dispositivo: {label}");
    let supported = device.default_input_config()?;
    let channels = usize::from(supported.channels());
    let rate = supported.sample_rate() as usize;
    let (raw_tx, raw_rx) = mpsc::sync_channel(16);
    let (pool_tx, pool_rx) = mpsc::sync_channel(16);
    for _ in 0..16 {
        pool_tx.send(Box::new([0.0; BLOCK_SAMPLES]))?;
    }
    let (error_tx, error_rx) = mpsc::sync_channel(4);
    let lost = Arc::new(AtomicUsize::new(0));
    let config = supported.config();
    macro_rules! stream {
        ($sample:ty) => {
            build_stream::<$sample>(&device, config, raw_tx, pool_rx, lost.clone(), error_tx)?
        };
    }
    let stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => stream!(f32),
        cpal::SampleFormat::F64 => stream!(f64),
        cpal::SampleFormat::I8 => stream!(i8),
        cpal::SampleFormat::I16 => stream!(i16),
        cpal::SampleFormat::I32 => stream!(i32),
        cpal::SampleFormat::I64 => stream!(i64),
        cpal::SampleFormat::U8 => stream!(u8),
        cpal::SampleFormat::U16 => stream!(u16),
        cpal::SampleFormat::U32 => stream!(u32),
        cpal::SampleFormat::U64 => stream!(u64),
        format => bail!("Formato de micrófono no compatible: {format}"),
    };
    let mut resampler = StreamingResampler::new(rate)?;
    let mut interleaved = Vec::new();
    stream.play()?;
    println!("Escuchando… (Ctrl+C para terminar)");
    while !stop.load(Ordering::Relaxed) {
        if let Ok(error) = error_rx.try_recv() {
            bail!("Error del micrófono: {error}");
        }
        match raw_rx.recv_timeout(Duration::from_millis(50)) {
            Ok((buffer, len)) => {
                interleaved.extend_from_slice(&buffer[..len]);
                pool_tx.try_send(buffer).ok();
                let complete = interleaved.len() / channels * channels;
                let samples = mono(&interleaved[..complete], channels);
                interleaved.drain(..complete);
                if !resampler.push(&samples, &tx)? {
                    return Ok(());
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        let dropped = lost.swap(0, Ordering::Relaxed);
        if dropped > 0 {
            eprintln!("Aviso: se han perdido {dropped} bloques de audio por sobrecarga");
        }
    }
    drop(stream);
    while let Ok((buffer, len)) = raw_rx.try_recv() {
        interleaved.extend_from_slice(&buffer[..len]);
    }
    let complete = interleaved.len() / channels * channels;
    resampler.push(&mono(&interleaved[..complete], channels), &tx)?;
    resampler.process(&tx, true)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pulse_sources_exclude_monitors_and_keep_default() {
        let sources = parse_pulse_sources(
            br#"[
            {"name":"usb", "description":"USB Microphone", "monitor_source":""},
            {"name":"sink.monitor", "description":"Playback"},
            {"name":"old_monitor", "monitor_of_sink":3},
            {"name":"other_monitor", "monitor_source":"sink"},
            {"name":"class_monitor", "properties":{"device.class":"monitor"}},
            {"name":"internal", "description":"Digital Microphone", "monitor_of_sink":4294967295},
            {"name":"usb", "description":"Duplicate"}
        ]"#,
            "usb\n",
        )
        .unwrap();
        assert_eq!(sources.len(), 2);
        assert_eq!(select_pulse_source(&sources, None).unwrap().name, "usb");
        assert_eq!(
            select_pulse_source(&sources, Some("USB MICRO"))
                .unwrap()
                .name,
            "usb"
        );
        assert_eq!(
            select_pulse_source(&sources, Some("internal"))
                .unwrap()
                .description,
            "Digital Microphone"
        );
        assert!(select_pulse_source(&sources, Some("microphone")).is_err());
        assert!(select_pulse_source(&sources, Some("monitor")).is_err());
        assert!(parse_pulse_sources(b"{}", "usb").is_err());
    }
    #[test]
    fn fallback_excludes_plugins_but_keeps_hardware_and_default() {
        assert_eq!(
            fallback_device_label("sof-hda-dsp, ", "hw:0,6", false).as_deref(),
            Some("sof-hda-dsp")
        );
        assert!(fallback_device_label("Rate Converter Plugin", "samplerate", false).is_none());
        assert!(fallback_device_label("JACK Audio Connection Kit", "jack", false).is_none());
        assert!(fallback_device_label("PulseAudio", "default", true).is_some());
    }
    #[test]
    fn streaming_resampler_keeps_tail_and_emits_exact_frames() {
        let input: Vec<_> = (0..4410).map(|i| (i as f32 * 0.1).sin() * 0.2).collect();
        let (tx, rx) = mpsc::sync_channel(16);
        let mut streaming = StreamingResampler::new(44100).unwrap();
        for chunk in input.chunks(317) {
            assert!(streaming.push(chunk, &tx).unwrap());
        }
        streaming.process(&tx, true).unwrap();
        drop(tx);
        let frames: Vec<_> = rx.into_iter().collect();
        assert!(frames.iter().all(|f| f.len() == FRAME_SAMPLES));
        let output: Vec<_> = frames.into_iter().flatten().collect();
        assert_eq!(output.len(), 2048);
        let mut resampler = Fft::<f32>::new(44100, 16000, 1024, 1, FixedSync::Input).unwrap();
        let adapter = InterleavedSlice::new(&input, 1, input.len()).unwrap();
        let reference = resampler
            .process_all(&adapter, input.len(), None)
            .unwrap()
            .take_data();
        assert_eq!(reference.len(), 1600);
        for (actual, expected) in output.iter().zip(&reference) {
            assert!((actual - expected).abs() < 1e-6);
        }
        assert!(output[1600..].iter().all(|s| *s == 0.0));
    }
    #[test]
    fn file_frames_pad_the_tail_and_honor_stop() {
        let (tx, rx) = mpsc::sync_channel(4);
        let stop = AtomicBool::new(false);
        send_frames(&vec![0.25; FRAME_SAMPLES + 1], &tx, &stop);
        let first = rx.try_recv().unwrap();
        let last = rx.try_recv().unwrap();
        assert_eq!(first, vec![0.25; FRAME_SAMPLES]);
        assert_eq!(last[0], 0.25);
        assert!(last[1..].iter().all(|s| *s == 0.0));
        stop.store(true, Ordering::Relaxed);
        send_frames(&[0.25; FRAME_SAMPLES], &tx, &stop);
        assert!(rx.try_recv().is_err());
    }
    #[test]
    fn stereo_wav_resamples_and_preserves_duration() {
        let path = std::env::temp_dir().join(format!("voz-audio-{}.wav", std::process::id()));
        let mut writer = hound::WavWriter::create(
            &path,
            hound::WavSpec {
                channels: 2,
                sample_rate: 48000,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            },
        )
        .unwrap();
        for _ in 0..4800 {
            writer.write_sample(8192_i16).unwrap();
            writer.write_sample(0_i16).unwrap();
        }
        writer.finalize().unwrap();
        let samples = read_wav_16k(&path).unwrap();
        std::fs::remove_file(path).unwrap();
        assert_eq!(samples.len(), 1600);
        assert!((samples[800] - 0.125).abs() < 0.01);
    }
}
