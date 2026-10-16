//! Captura del audio que suena en el PC (una aplicación concreta o todo), mono 16 kHz en tramas de FRAME_SAMPLES.
//!
//! Sin `--app` se graba el monitor de la salida predeterminada. Con `--app` se graban uno a uno
//! los streams de reproducción que la mencionen (`parecord --monitor-stream`), mezclándolos; cada
//! ~1 s se enganchan los nuevos y se sueltan los que desaparecen, sin cortar.

use crate::ducking::VolumeGuard;
use crate::types::FRAME_SAMPLES;
use anyhow::{Context, Result, bail};
use std::{
    collections::{HashMap, HashSet, VecDeque, hash_map::Entry},
    io::Read,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender},
    },
    time::{Duration, Instant},
};

/// Todo lo que contenga esto en nombre/binario/medio es del propio programa y no se captura.
const OWN_MARKER: &str = "voz-traductor";
/// Muestras por bloque que entrega cada lector (128 ms).
const BLOCK_SAMPLES: usize = 2048;
/// Cuánto puede atrasarse un stream antes de tratarlo como silencioso (en pausa, suspendido).
const STALL_AFTER: Duration = Duration::from_millis(500);
/// Tope de cada cola por stream (se tira lo más viejo).
const MAX_BUFFERED: usize = 16_000 * 5;

#[derive(Debug, Clone)]
struct PlaybackStream {
    index: u32,
    app_name: String,
    binary: String,
    media_name: String,
    volumes: Vec<u32>,
    muted: bool,
}

impl PlaybackStream {
    fn haystack(&self) -> String {
        format!("{}\n{}\n{}", self.app_name, self.binary, self.media_name).to_lowercase()
    }
}

/// Volúmenes por canal de un stream o de un grabador de Pulse, en el orden de `channel_map`.
pub(crate) fn channel_volumes(value: &serde_json::Value) -> Result<Vec<u32>> {
    let channels = value["channel_map"]
        .as_str()
        .context("Stream sin canales")?;
    channels
        .split(',')
        .map(|channel| {
            value["volume"][channel]["value"]
                .as_u64()
                .map(|volume| volume as u32)
                .context("Volumen de stream no válido")
        })
        .collect()
}

fn is_own(haystack: &str) -> bool {
    haystack.contains(OWN_MARKER)
}

/// Streams de reproducción actuales (`pactl -f json list sink-inputs`).
fn list_playback_streams() -> Result<Vec<PlaybackStream>> {
    let output = Command::new("pactl")
        .args(["-f", "json", "list", "sink-inputs"])
        .output()
        .context("No se puede ejecutar pactl; instala pulseaudio-utils")?;
    if !output.status.success() {
        bail!(
            "pactl list sink-inputs falló: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("Respuesta JSON de pactl no válida")?;
    let items = value.as_array().context("pactl no devolvió una lista")?;
    let mut streams = Vec::new();
    for item in items {
        let index = item["index"].as_u64().context("Stream Pulse sin índice")? as u32;
        let props = &item["properties"];
        let get = |key: &str| props[key].as_str().unwrap_or("").to_owned();
        streams.push(PlaybackStream {
            index,
            app_name: get("application.name"),
            binary: get("application.process.binary"),
            media_name: get("media.name"),
            volumes: channel_volumes(item)?,
            muted: item["mute"].as_bool().unwrap_or(false),
        });
    }
    Ok(streams)
}

/// Snapshot de los mismos streams que selecciona la captura, con sus volúmenes.
pub(crate) fn captured_streams(app: &str) -> Result<Vec<serde_json::Value>> {
    let output = Command::new("pactl")
        .args(["-f", "json", "list", "sink-inputs"])
        .output()?;
    anyhow::ensure!(
        output.status.success(),
        "No se pueden consultar los streams capturados"
    );
    let items: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout)?;
    Ok(items
        .into_iter()
        .filter(|item| {
            let props = &item["properties"];
            let stream = PlaybackStream {
                index: 0,
                app_name: props["application.name"]
                    .as_str()
                    .unwrap_or_default()
                    .into(),
                binary: props["application.process.binary"]
                    .as_str()
                    .unwrap_or_default()
                    .into(),
                media_name: props["media.name"].as_str().unwrap_or_default().into(),
                volumes: Vec::new(),
                muted: false,
            };
            !wanted(&[stream], Some(app)).is_empty()
        })
        .collect())
}

/// Los que hay que grabar: contienen `app` (o todos sin filtro) y nunca los propios.
fn wanted(streams: &[PlaybackStream], app: Option<&str>) -> Vec<PlaybackStream> {
    let query = app.map(|a| a.to_lowercase());
    streams
        .iter()
        .filter(|s| {
            let hay = s.haystack();
            if is_own(&hay) {
                return false;
            }
            match &query {
                None => true,
                Some(q) => hay.contains(q.as_str()),
            }
        })
        .cloned()
        .collect()
}

pub(crate) fn capture_stream_name(index: &str) -> String {
    format!("voz-traductor-oir-{}-{index}", std::process::id())
}

fn spawn_recorder(device_args: &[String]) -> Result<Child> {
    let mut command = Command::new("parecord");
    command.args([
        "--raw",
        "--format=float32le",
        "--rate=16000",
        "--channels=1",
        "--latency-msec=50",
    ]);
    command.arg("--volume=65536");
    if let Some(index) = device_args
        .iter()
        .find_map(|arg| arg.strip_prefix("--monitor-stream="))
    {
        command.arg(format!("--stream-name={}", capture_stream_name(index)));
        command.arg(format!("--client-name={}", capture_stream_name(index)));
        // El ajuste de captura del doblaje no debe quedar recordado para otro
        // proceso ni para otro stream (WirePlumber puede sobrescribir --volume).
        command.arg(format!(
            "--property=module-stream-restore.id=source-output-by-application-name:{}",
            capture_stream_name(index)
        ));
    }
    if !device_args
        .iter()
        .any(|arg| arg.starts_with("--monitor-stream="))
    {
        let name = capture_stream_name("monitor");
        command.arg(format!("--client-name={name}"));
        command.arg(format!("--stream-name={name}"));
        command.arg(format!(
            "--property=module-stream-restore.id=source-output-by-application-name:{name}"
        ));
    }
    for arg in device_args {
        command.arg(arg);
    }
    command
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("No se puede iniciar parecord; instala pulseaudio-utils")
}

/// Mezcla una trama de `FRAME_SAMPLES`: suma lo que haya por stream (con tope en ±1).
/// Devuelve `None` si no hay una trama completa lista (para no picar el audio con ceros).
fn mix_one_frame(
    buffers: &mut HashMap<u32, VecDeque<f32>>,
    stalled: &HashSet<u32>,
) -> Option<Vec<f32>> {
    if buffers.is_empty() {
        return None;
    }
    let mut any_data = false;
    for (key, buffer) in buffers.iter() {
        if buffer.len() >= FRAME_SAMPLES {
            any_data = true;
        } else if !stalled.contains(key) {
            return None;
        }
    }
    if !any_data {
        return None;
    }
    let mut frame = vec![0.0; FRAME_SAMPLES];
    for buffer in buffers.values_mut() {
        let take = buffer.len().min(FRAME_SAMPLES);
        for (dst, src) in frame.iter_mut().zip(buffer.drain(..take)) {
            *dst += src;
        }
    }
    for s in frame.iter_mut() {
        *s = s.clamp(-1.0, 1.0);
    }
    Some(frame)
}

struct Tap {
    child: Child,
    reader: Option<std::thread::JoinHandle<()>>,
    last_data: Instant,
    dead: bool,
}

fn spawn_tap(
    key: u32,
    device_args: &[String],
    blocks: mpsc::Sender<(u32, Option<Vec<f32>>)>,
) -> Result<Tap> {
    let mut child = spawn_recorder(device_args)?;
    let stdout = child.stdout.take().context("parecord sin salida")?;
    let reader = std::thread::spawn(move || read_blocks(key, stdout, blocks));
    Ok(Tap {
        child,
        reader: Some(reader),
        last_data: Instant::now(),
        dead: false,
    })
}

fn read_blocks(
    key: u32,
    mut stdout: impl Read + Send + 'static,
    blocks: mpsc::Sender<(u32, Option<Vec<f32>>)>,
) {
    let mut tail = Vec::new();
    let mut chunk = [0u8; BLOCK_SAMPLES * 4];
    loop {
        match stdout.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                tail.extend_from_slice(&chunk[..n]);
                let samples = tail.len() / 4;
                if samples > 0 {
                    let mut block = Vec::with_capacity(samples);
                    for i in 0..samples {
                        let b = [
                            tail[4 * i],
                            tail[4 * i + 1],
                            tail[4 * i + 2],
                            tail[4 * i + 3],
                        ];
                        block.push(f32::from_le_bytes(b));
                    }
                    tail.drain(..samples * 4);
                    if blocks.send((key, Some(block))).is_err() {
                        break;
                    }
                }
            }
            Err(_) => break,
        }
    }
    let _ = blocks.send((key, None));
}

fn stop_tap(tap: &mut Tap) {
    tap.child.kill().ok();
    tap.child.wait().ok();
    if let Some(reader) = tap.reader.take() {
        reader.join().ok();
    }
}

/// Graba el audio del PC en tramas de `FRAME_SAMPLES` hasta que `stop` se active.
/// Con `app`, solo los streams que la mencionen (mezclados); sin `app`, todo el monitor.
/// `on_status` recibe avisos breves para la ventana ("Esperando audio de …").
///
/// Con `--app` la captura no depende del volumen del stream: `guard` compensa en el
/// source-output propio la ganancia del sink-input (con tope) y restaura los volúmenes
/// que quedaron bajados de una atenuación anterior. Sin `--app` solo se observan los
/// volúmenes para restaurar los pendientes (el monitor mezcla todos los streams).
pub fn capture_system_audio(
    app: Option<&str>,
    tx: SyncSender<Vec<f32>>,
    stop: &Arc<AtomicBool>,
    on_status: &dyn Fn(String),
    guard: &Mutex<VolumeGuard>,
) -> Result<()> {
    Command::new("parecord")
        .arg("--version")
        .output()
        .context("No se puede ejecutar parecord; instala pulseaudio-utils")?;
    // Clave del monitor general (reutilizada en cada revisión).
    const MONITOR_KEY: u32 = u32::MAX;
    let monitor_args = vec!["--device=@DEFAULT_MONITOR@".to_owned()];

    let (blocks_tx, blocks_rx) = mpsc::channel::<(u32, Option<Vec<f32>>)>();
    let mut taps: HashMap<u32, Tap> = HashMap::new();
    let mut buffers: HashMap<u32, VecDeque<f32>> = HashMap::new();
    let mut had_streams = false;
    let mut waiting_since: Option<Instant> = None;
    let mut last_watch = Instant::now() - Duration::from_secs(10);

    let mut running = true;
    while running && !stop.load(Ordering::Relaxed) {
        if last_watch.elapsed() >= Duration::from_secs(1) {
            last_watch = Instant::now();
            match list_playback_streams() {
                Ok(streams) => {
                    let selected = wanted(&streams, app);
                    {
                        let mut guard = guard.lock().unwrap_or_else(|error| error.into_inner());
                        // Volúmenes que quedaron bajados de una atenuación anterior.
                        for stream in &streams {
                            if let Err(error) = guard.restore_pending(
                                stream.index,
                                &stream.app_name,
                                &stream.binary,
                                &stream.volumes,
                            ) {
                                eprintln!(
                                    "Aviso: no se pudo comprobar el volumen de {}: {error:#}",
                                    stream.app_name
                                );
                            }
                        }
                    }
                    if app.is_none() {
                        if let Entry::Vacant(slot) = taps.entry(MONITOR_KEY) {
                            match spawn_tap(MONITOR_KEY, &monitor_args, blocks_tx.clone()) {
                                Ok(tap) => {
                                    slot.insert(tap);
                                }
                                Err(error) => bail!("No se puede grabar el monitor: {error:#}"),
                            }
                        }
                    } else {
                        for stream in &selected {
                            if let Entry::Vacant(slot) = taps.entry(stream.index) {
                                let args = vec![format!("--monitor-stream={}", stream.index)];
                                match spawn_tap(stream.index, &args, blocks_tx.clone()) {
                                    Ok(tap) => {
                                        slot.insert(tap);
                                    }
                                    Err(error) => {
                                        eprintln!(
                                            "Aviso: no se puede enganchar el stream {}: {error:#}",
                                            stream.index
                                        );
                                    }
                                }
                            }
                        }
                        // Compensación del volumen del stream: se refresca en cada revisión
                        // (el doblaje la refresca además en cada tick, al atenuar).
                        let mut guard = guard.lock().unwrap_or_else(|error| error.into_inner());
                        for stream in &selected {
                            if !taps.contains_key(&stream.index) {
                                continue;
                            }
                            if let Err(error) = guard.capture_gain(
                                stream.index,
                                &stream.volumes,
                                stream.muted,
                                &stream.app_name,
                                &stream.binary,
                            ) {
                                eprintln!(
                                    "Aviso: no se pudo compensar el volumen de {}: {error:#}",
                                    stream.app_name
                                );
                            }
                        }
                    }
                    // Lo desaparecido se marca para soltarlo cuando se drene su resto (sin cortar).
                    let live: HashSet<u32> = if app.is_none() {
                        [MONITOR_KEY].into_iter().collect()
                    } else {
                        selected.iter().map(|stream| stream.index).collect()
                    };
                    for (key, tap) in taps.iter_mut() {
                        if !live.contains(key) {
                            tap.dead = true;
                        }
                    }
                    let n_live = taps.values().filter(|tap| !tap.dead).count();
                    if app.is_some() {
                        if n_live == 0 {
                            let remind =
                                waiting_since.is_none_or(|t| t.elapsed() > Duration::from_secs(3));
                            if remind {
                                waiting_since = Some(Instant::now());
                                on_status(format!("Esperando audio de {}…", app.unwrap_or("")));
                            }
                        } else {
                            waiting_since = None;
                            if !had_streams {
                                on_status(format!("Captando audio de {}…", app.unwrap_or("")));
                            }
                        }
                    }
                    had_streams = n_live > 0;
                }
                Err(error) => eprintln!("Aviso: no se puede listar el audio del PC: {error:#}"),
            }
            // Recoger hijos muertos.
            for tap in taps.values_mut() {
                if !tap.dead
                    && let Ok(Some(_)) = tap.child.try_wait()
                {
                    tap.dead = true;
                }
            }
        }
        match blocks_rx.recv_timeout(Duration::from_millis(200)) {
            Ok((key, Some(block))) => {
                if taps.contains_key(&key) {
                    let buffer = buffers.entry(key).or_default();
                    buffer.extend(block);
                    if buffer.len() > MAX_BUFFERED {
                        buffer.drain(..buffer.len() - MAX_BUFFERED);
                    }
                    if let Some(tap) = taps.get_mut(&key) {
                        tap.last_data = Instant::now();
                    }
                }
            }
            Ok((key, None)) => {
                if let Some(tap) = taps.get_mut(&key) {
                    tap.dead = true;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        // Quitar lo muerto cuando ya se drenó su resto.
        taps.retain(|key, tap| {
            let drained = buffers.get(key).is_none_or(|b| b.is_empty());
            if tap.dead && drained {
                buffers.remove(key);
                guard
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .release(*key);
                stop_tap(tap);
                false
            } else {
                true
            }
        });
        let now = Instant::now();
        let stalled: HashSet<u32> = taps
            .iter()
            .filter(|(_, tap)| now.duration_since(tap.last_data) > STALL_AFTER)
            .map(|(key, _)| *key)
            .collect();
        while let Some(frame) = mix_one_frame(&mut buffers, &stalled) {
            if tx.send(frame).is_err() {
                running = false;
                break;
            }
        }
    }
    for (_, mut tap) in taps {
        stop_tap(&mut tap);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_streams() -> Vec<PlaybackStream> {
        vec![
            PlaybackStream {
                index: 7,
                app_name: "Firefox".into(),
                binary: "firefox".into(),
                media_name: "Audio de la pestaña".into(),
                volumes: vec![65536],
                muted: false,
            },
            PlaybackStream {
                index: 9,
                app_name: "voz-traductor-virtual".into(),
                binary: "pacat".into(),
                media_name: "Traducción".into(),
                volumes: vec![65536],
                muted: false,
            },
            PlaybackStream {
                index: 11,
                app_name: "WEBRTC VoiceEngine".into(),
                binary: "discord".into(),
                media_name: "voz".into(),
                volumes: vec![65536],
                muted: false,
            },
        ]
    }

    #[test]
    fn filtra_por_app_y_excluye_lo_propio() {
        let streams = sample_streams();
        let fire = wanted(&streams, Some("fire"));
        assert_eq!(fire.len(), 1);
        assert_eq!(fire[0].index, 7);
        let voice = wanted(&streams, Some("VOICEENGINE"));
        assert_eq!(voice.len(), 1);
        assert_eq!(voice[0].index, 11);
        // Sin filtro coge todo menos lo propio.
        let all = wanted(&streams, None);
        assert_eq!(all.len(), 2);
        // Lo propio no se coge ni pidiéndolo.
        assert!(wanted(&streams, Some("voz-traductor")).is_empty());
        assert!(wanted(&streams, Some("pacat")).is_empty());
    }

    #[test]
    fn volumenes_por_canal_en_orden() {
        let value = serde_json::json!({
            "channel_map": "front-left,front-right",
            "volume": {"front-right": {"value": 23457}, "front-left": {"value": 49151}}
        });
        assert_eq!(channel_volumes(&value).unwrap(), [49151, 23457]);
        assert!(channel_volumes(&serde_json::json!({"channel_map": "mono"})).is_err());
    }

    #[test]
    fn mezcla_suma_y_no_pica() {
        let mut buffers: HashMap<u32, VecDeque<f32>> = HashMap::new();
        buffers.insert(1, vec![0.5; FRAME_SAMPLES].into());
        buffers.insert(2, vec![0.25; FRAME_SAMPLES].into());
        let frame = mix_one_frame(&mut buffers, &HashSet::new()).unwrap();
        assert!(frame.iter().all(|s| (*s - 0.75).abs() < 1e-6));
        // Sin trama completa no emite nada.
        buffers.insert(1, vec![0.5; 100].into());
        buffers.insert(2, VecDeque::new());
        assert!(mix_one_frame(&mut buffers, &HashSet::new()).is_none());
        // Con el parado marcado, rellena con ceros y emite.
        let stalled: HashSet<u32> = [2].into_iter().collect();
        buffers.insert(1, vec![0.5; FRAME_SAMPLES].into());
        let frame = mix_one_frame(&mut buffers, &stalled).unwrap();
        assert!(frame.iter().all(|s| (*s - 0.5).abs() < 1e-6));
        // Todo parado: no emite silencio infinito.
        buffers.clear();
        buffers.insert(2, VecDeque::new());
        assert!(mix_one_frame(&mut buffers, &stalled).is_none());
    }
}
