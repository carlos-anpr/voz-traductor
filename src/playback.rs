//! Reproducción ordenada fuera del pipeline, un proceso y un hilo por destino.
use crate::{config::PlaybackConfig, virtual_mic};
use anyhow::{Context, Result, anyhow, ensure};
use std::{
    io::Write,
    process::{Child, Command, Stdio},
    sync::{Arc, mpsc},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[derive(Clone, Debug, serde::Deserialize)]
struct Sink {
    name: String,
    description: String,
}
impl Sink {
    fn display(&self) -> String {
        format!("{} [{}]", self.description, self.name)
    }
}
fn sinks() -> Result<Vec<Sink>> {
    let output = Command::new("pactl")
        .args(["-f", "json", "list", "sinks"])
        .output()
        .context("No se pueden consultar las salidas; instala pulseaudio-utils")?;
    ensure!(
        output.status.success(),
        "No se pueden consultar las salidas: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut sinks: Vec<Sink> = serde_json::from_slice(&output.stdout)
        .context("Respuesta de pactl no válida al listar salidas")?;
    sinks.retain(|sink| sink.name != virtual_mic::SINK);
    Ok(sinks)
}
fn select_sink(query: &str, sinks: &[Sink]) -> Result<Sink> {
    let query_lower = query.to_lowercase();
    let matches: Vec<_> = sinks
        .iter()
        .filter(|sink| {
            sink.name.to_lowercase().contains(&query_lower)
                || sink.description.to_lowercase().contains(&query_lower)
        })
        .collect();
    if !query.trim().is_empty() && matches.len() == 1 {
        return Ok(matches[0].clone());
    }
    let reason = if matches.len() > 1 {
        "Coincidencia ambigua"
    } else {
        "Ninguna coincidencia"
    };
    anyhow::bail!(
        "{reason} para auriculares {query:?}. Salidas disponibles:\n{}",
        sinks
            .iter()
            .map(Sink::display)
            .collect::<Vec<_>>()
            .join("\n")
    );
}
pub fn list_outputs() -> Result<()> {
    let sinks = sinks()?;
    let output = Command::new("pactl").arg("get-default-sink").output()?;
    ensure!(
        output.status.success(),
        "No se puede consultar la salida predeterminada"
    );
    let default = String::from_utf8_lossy(&output.stdout);
    for sink in sinks {
        println!(
            "{}{}",
            sink.display(),
            if sink.name == default.trim() {
                " (predeterminada)"
            } else {
                ""
            }
        );
    }
    Ok(())
}

/// Una desconexión cambia la ruta hasta terminar la sesión, aunque vuelva el sink.
struct MonitorRoute {
    selected: Option<Sink>,
    last_check: Instant,
}
impl MonitorRoute {
    fn disconnected(&mut self, available: &[Sink]) -> bool {
        if self
            .selected
            .as_ref()
            .is_some_and(|selected| !available.iter().any(|sink| sink.name == selected.name))
        {
            let selected = self.selected.take().expect("salida seleccionada");
            eprintln!(
                "Aviso: {} ya no está disponible; se continúa por la salida predeterminada hasta terminar la sesión.",
                selected.display()
            );
            return true;
        }
        false
    }
    fn prepare(&mut self, destination: Destination, child: &mut Option<Child>) -> Result<()> {
        if matches!(destination, Destination::Headphones)
            && self.selected.is_some()
            && (child.is_none() || self.last_check.elapsed() >= Duration::from_millis(500))
        {
            self.last_check = Instant::now();
            // Un fallo de consulta no demuestra que hayan desaparecido los cascos.
            if let Ok(available) = sinks()
                && self.disconnected(&available)
            {
                terminate(child);
            }
        }
        if child.is_none() {
            *child = Some(spawn_pacat(
                destination,
                self.selected.as_ref().map(|sink| sink.name.as_str()),
            )?);
        }
        Ok(())
    }
}

type Audio = Arc<[f32]>;
#[derive(Clone, Copy)]
enum Destination {
    VirtualMic,
    Headphones,
    Dubbing,
}
impl Destination {
    fn label(self) -> &'static str {
        match self {
            Self::VirtualMic => "micrófono virtual",
            Self::Headphones => "auriculares",
            Self::Dubbing => "doblaje",
        }
    }
}
fn spawn_pacat(destination: Destination, monitor: Option<&str>) -> Result<Child> {
    if matches!(destination, Destination::VirtualMic) {
        virtual_mic::ensure()?;
    }
    let mut command = Command::new("pacat");
    command.args([
        "--playback",
        "--raw",
        "--format=float32le",
        "--rate=24000",
        "--channels=1",
        "--latency-msec=40",
        "--process-time-msec=10",
    ]);
    command.arg(format!("--stream-name=Traducción: {}", destination.label()));
    let (identity, device) = match destination {
        Destination::VirtualMic => ("voz-traductor-virtual", virtual_mic::SINK),
        Destination::Headphones => ("voz-traductor-monitor", monitor.unwrap_or("@DEFAULT_SINK@")),
        Destination::Dubbing => ("voz-traductor-doblaje", monitor.unwrap_or("@DEFAULT_SINK@")),
    };
    command.arg(format!("--client-name={identity}"));
    command.arg(format!(
        "--property=module-stream-restore.id=sink-input-by-application-name:{identity}"
    ));
    command.arg(format!("--device={device}"));
    // Ctrl+C llega al proceso principal; pacat debe poder drenar la cola.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| {
            format!(
                "No se puede iniciar pacat para {}; instala pulseaudio-utils",
                destination.label()
            )
        })
}
fn terminate(child: &mut Option<Child>) {
    if let Some(mut process) = child.take() {
        drop(process.stdin.take());
        process.kill().ok();
        process.wait().ok();
    }
}

/// Salida síncrona para que la cola de doblaje conozca lo pendiente de sonar.
/// Un pacat por trozo drena exactamente ese audio, sin otra cola oculta.
pub struct DubOutput {
    sink: Sink,
    child: Option<Child>,
}
impl DubOutput {
    pub fn new(query: Option<&str>) -> Result<Self> {
        let available = sinks()?;
        let sink = if let Some(query) = query {
            select_sink(query, &available)?
        } else {
            let output = Command::new("pactl").arg("get-default-sink").output()?;
            ensure!(
                output.status.success(),
                "No se puede consultar la salida predeterminada"
            );
            let name = String::from_utf8(output.stdout)?;
            available
                .into_iter()
                .find(|sink| sink.name == name.trim())
                .context("La salida predeterminada no está disponible para doblaje")?
        };
        println!("Salida de doblaje: {}", sink.display());
        Ok(Self { sink, child: None })
    }

    pub fn play(
        &mut self,
        audio: &[f32],
        stop: &std::sync::atomic::AtomicBool,
        mut tick: impl FnMut() -> Result<()>,
    ) -> Result<bool> {
        use std::sync::atomic::Ordering;
        ensure!(
            sinks()?.iter().any(|sink| sink.name == self.sink.name),
            "La salida de doblaje desapareció; se detiene para evitar reproducir por otra salida"
        );
        self.child = Some(spawn_pacat(Destination::Dubbing, Some(&self.sink.name))?);
        let mut input = self
            .child
            .as_mut()
            .context("Sin pacat de doblaje")?
            .stdin
            .take()
            .context("Sin entrada de pacat")?;
        thread::scope(|scope| {
            let writer = scope.spawn(move || -> Result<()> {
                for chunk in audio.chunks(4096) {
                    let bytes: Vec<_> = chunk
                        .iter()
                        .flat_map(|sample| sample.to_le_bytes())
                        .collect();
                    input.write_all(&bytes)?;
                }
                Ok(())
            });
            let result = (|| -> Result<bool> {
                loop {
                    if stop.load(Ordering::Relaxed) {
                        return Ok(false);
                    }
                    if let Err(error) = tick() {
                        if stop.load(Ordering::Relaxed) {
                            return Ok(false);
                        }
                        return Err(error);
                    }
                    if let Some(status) = self
                        .child
                        .as_mut()
                        .context("Sin pacat de doblaje")?
                        .try_wait()?
                    {
                        ensure!(status.success(), "pacat de doblaje terminó con {status}");
                        return Ok(true);
                    }
                    thread::sleep(Duration::from_millis(20));
                }
            })();
            // Desbloquea también al escritor si pacat deja de leer o llega una señal.
            terminate(&mut self.child);
            let written = writer
                .join()
                .map_err(|_| anyhow!("El escritor de doblaje terminó inesperadamente"))?;
            if matches!(result, Ok(true)) {
                written?;
            }
            result
        })
    }
}
impl Drop for DubOutput {
    fn drop(&mut self) {
        terminate(&mut self.child);
    }
}
fn play_queue(
    rx: mpsc::Receiver<Audio>,
    mut child: Option<Child>,
    mut prepare: impl FnMut(&mut Option<Child>) -> Result<()>,
    label: &str,
) -> Result<()> {
    loop {
        if let Some(process) = &mut child
            && process.try_wait()?.is_some()
        {
            eprintln!("Aviso: pacat de {label} se ha detenido; se relanza.");
            terminate(&mut child);
        }
        if let Err(error) = prepare(&mut child) {
            eprintln!("No se puede reconectar la reproducción en {label}: {error:#}");
            thread::sleep(Duration::from_millis(300));
        }
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(audio) => {
                for attempt in 0..2 {
                    let result = (|| -> Result<()> {
                        for chunk in audio.chunks(4096) {
                            prepare(&mut child)?;
                            let input = child
                                .as_mut()
                                .context("No hay proceso de reproducción")?
                                .stdin
                                .as_mut()
                                .context("pacat no tiene entrada de audio")?;
                            let bytes: Vec<_> = chunk
                                .iter()
                                .flat_map(|sample| sample.to_le_bytes())
                                .collect();
                            input.write_all(&bytes)?;
                        }
                        Ok(())
                    })();
                    match result {
                        Ok(()) => break,
                        Err(error) => {
                            eprintln!(
                                "Error de reproducción en {label}: {error:#}{}",
                                if attempt == 0 {
                                    "; se relanza pacat y se reintenta la frase"
                                } else {
                                    "; el audio sigue disponible en la sesión"
                                }
                            );
                            terminate(&mut child);
                        }
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    if child.is_some() {
        // Seguir vigilando los cascos también mientras pacat drena tras EOF.
        loop {
            let process = child
                .as_mut()
                .context("No hay proceso al drenar la salida")?;
            drop(process.stdin.take());
            if let Some(status) = process.try_wait()? {
                ensure!(status.success(), "pacat de {label} terminó con {status}");
                break;
            }
            prepare(&mut child)?;
            thread::sleep(Duration::from_millis(20));
        }
    }
    Ok(())
}

pub struct Playback {
    senders: Vec<mpsc::Sender<Audio>>,
    workers: Vec<JoinHandle<Result<()>>>,
}
impl Playback {
    pub fn start(cfg: &PlaybackConfig) -> Result<Self> {
        let monitor_enabled = cfg.monitor || cfg.monitor_device.is_some();
        let selected = cfg
            .monitor_device
            .as_deref()
            .map(|query| select_sink(query, &sinks()?))
            .transpose()?;
        if cfg.virtual_mic {
            let created = virtual_mic::ensure()?;
            if created && !virtual_mic::permanent_path()?.is_file() {
                eprintln!(
                    "Micrófono virtual creado. Para hacerlo permanente: voz-traductor microfono instalar"
                );
            }
        }
        let mut playback = Self {
            senders: Vec::new(),
            workers: Vec::new(),
        };
        let mut destinations = Vec::new();
        if cfg.virtual_mic {
            destinations.push(Destination::VirtualMic);
        }
        if monitor_enabled {
            destinations.push(Destination::Headphones);
        }
        for destination in destinations {
            let mut route = MonitorRoute {
                selected: selected.clone(),
                last_check: Instant::now(),
            };
            let (tx, rx) = mpsc::channel();
            let (ready_tx, ready_rx) = mpsc::sync_channel(1);
            let worker = thread::Builder::new()
                .name(format!("salida-{}", destination.label()))
                .spawn(move || {
                    let mut child = None;
                    match route.prepare(destination, &mut child) {
                        Ok(()) => {
                            ready_tx.send(Ok(())).ok();
                        }
                        Err(error) => {
                            ready_tx.send(Err(format!("{error:#}"))).ok();
                            return Err(error);
                        }
                    }
                    play_queue(
                        rx,
                        child,
                        |child| route.prepare(destination, child),
                        destination.label(),
                    )
                })?;
            playback.senders.push(tx);
            playback.workers.push(worker);
            ready_rx
                .recv()
                .context("No se pudo iniciar la reproducción")?
                .map_err(|error| anyhow!(error))?;
        }
        let label = match (cfg.virtual_mic, monitor_enabled) {
            (true, true) => "Salida: micrófono virtual \"Voz traducida\" + auriculares",
            (true, false) => "Salida: micrófono virtual \"Voz traducida\"",
            (false, true) => "Modo prueba: auriculares",
            (false, false) => "Salida: solo sesión en disco",
        };
        println!(
            "{label}{}",
            selected
                .as_ref()
                .map(|sink| format!(": {}", sink.display()))
                .unwrap_or_default()
        );
        Ok(playback)
    }
    /// Encolar nunca espera a pacat ni a que termine de sonar la frase anterior.
    pub fn enqueue(&self, audio: Vec<f32>) {
        if self.senders.is_empty() {
            return;
        }
        let audio: Audio = audio.into();
        for sender in &self.senders {
            if sender.send(audio.clone()).is_err() {
                eprintln!(
                    "Error: una salida de reproducción ya no está disponible; el audio está guardado en disco."
                );
            }
        }
    }
    fn join(&mut self) -> Result<()> {
        self.senders.clear();
        let mut failure = None;
        for worker in self.workers.drain(..) {
            let result = worker
                .join()
                .map_err(|_| anyhow!("El hilo de reproducción terminó inesperadamente"))
                .and_then(|r| r);
            if let Err(error) = result {
                failure = Some(error);
            }
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
    pub fn finish(mut self) -> Result<()> {
        self.join()
    }
}
impl Drop for Playback {
    fn drop(&mut self) {
        if let Err(error) = self.join() {
            eprintln!("Error al cerrar la reproducción: {error:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sink(name: &str, description: &str) -> Sink {
        Sink {
            name: name.into(),
            description: description.into(),
        }
    }
    #[test]
    fn selection_matches_names_and_descriptions_and_reports_all_choices() {
        let sinks = vec![
            sink("bluez_output.123", "Sound Blaster JAM V2"),
            sink("hdmi", "Pantalla"),
        ];
        assert_eq!(select_sink("jAm", &sinks).unwrap().name, "bluez_output.123");
        assert_eq!(
            select_sink("BLUEZ", &sinks).unwrap().name,
            "bluez_output.123"
        );
        for query in ["missing", "", "a"] {
            let error = select_sink(query, &sinks).unwrap_err().to_string();
            assert!(error.contains("Sound Blaster JAM V2 [bluez_output.123]"));
            assert!(error.contains("Pantalla [hdmi]"));
        }
        assert!(
            select_sink("a", &sinks)
                .unwrap_err()
                .to_string()
                .contains("ambigua")
        );
    }
    #[test]
    fn disconnected_headphones_switch_once_and_do_not_reattach() {
        let selected = sink("jam", "Cascos");
        let mut route = MonitorRoute {
            selected: Some(selected.clone()),
            last_check: Instant::now(),
        };
        assert!(!route.disconnected(std::slice::from_ref(&selected)));
        assert!(route.disconnected(&[]));
        assert!(route.selected.is_none());
        assert!(!route.disconnected(&[]));
        assert!(!route.disconnected(&[selected]));
        assert!(route.selected.is_none());
    }

    #[test]
    #[ignore = "Prueba Pulse real con silencio y sink temporal; no cambia el predeterminado"]
    fn pulse_disconnect_preserves_virtual_and_falls_back_to_default() {
        struct Module(String);
        impl Drop for Module {
            fn drop(&mut self) {
                Command::new("pactl")
                    .args(["unload-module", &self.0])
                    .output()
                    .ok();
            }
        }
        fn json(kind: &str) -> serde_json::Value {
            let output = Command::new("pactl")
                .args(["-f", "json", "list", kind])
                .output()
                .unwrap();
            assert!(output.status.success());
            serde_json::from_slice(&output.stdout).unwrap()
        }
        fn stream_destinations() -> Vec<(String, u64, String)> {
            json("sink-inputs")
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|stream| {
                    let props = &stream["properties"];
                    let name = props["application.name"].as_str()?;
                    name.starts_with("voz-traductor-").then(|| {
                        (
                            name.to_owned(),
                            stream["sink"].as_u64().unwrap(),
                            props["module-stream-restore.id"]
                                .as_str()
                                .unwrap_or_default()
                                .to_owned(),
                        )
                    })
                })
                .collect()
        }
        fn wait_for(expected_monitor: u64, expected_virtual: u64) {
            for _ in 0..60 {
                let streams = stream_destinations();
                if streams.iter().any(|(name, sink, _)| {
                    name == "voz-traductor-monitor" && *sink == expected_monitor
                }) && streams.iter().any(|(name, sink, _)| {
                    name == "voz-traductor-virtual" && *sink == expected_virtual
                }) {
                    assert!(streams.iter().all(
                        |(name, _, id)| id == &format!("sink-input-by-application-name:{name}")
                    ));
                    eprintln!("Destinos comprobados: {streams:?}");
                    return;
                }
                thread::sleep(Duration::from_millis(50));
            }
            panic!(
                "No aparecieron los destinos esperados: {:?}",
                stream_destinations()
            );
        }
        let default = Command::new("pactl")
            .arg("get-default-sink")
            .output()
            .unwrap()
            .stdout;
        let name = format!("voz-prueba-auriculares-{}", std::process::id());
        let output = Command::new("pactl")
            .args([
                "load-module",
                "module-null-sink",
                &format!("sink_name={name}"),
            ])
            .output()
            .unwrap();
        assert!(output.status.success());
        let module = Module(String::from_utf8(output.stdout).unwrap().trim().to_owned());
        let available = json("sinks");
        let index = |name: &str| {
            available
                .as_array()
                .unwrap()
                .iter()
                .find(|sink| sink["name"] == name)
                .unwrap()["index"]
                .as_u64()
                .unwrap()
        };
        let temporary_index = index(&name);
        let virtual_index = index(virtual_mic::SINK);
        let default_index = index(String::from_utf8_lossy(&default).trim());
        let playback = Playback::start(&PlaybackConfig {
            virtual_mic: true,
            monitor: true,
            monitor_device: Some(name),
        })
        .unwrap();
        playback.enqueue(vec![0.0; 24000 * 5]);
        wait_for(temporary_index, virtual_index);
        drop(module);
        wait_for(default_index, virtual_index);
        playback.finish().unwrap();
        assert_eq!(
            Command::new("pactl")
                .arg("get-default-sink")
                .output()
                .unwrap()
                .stdout,
            default
        );
    }

    #[test]
    fn ordered_queue_drains_and_restarts_a_dead_process() {
        let path = std::env::temp_dir().join(format!("voz-playback-{}.raw", std::process::id()));
        let output = std::fs::File::create(&path).unwrap();
        let (tx, rx) = mpsc::channel();
        tx.send(Arc::from(vec![0.25_f32, 0.5])).unwrap();
        tx.send(Arc::from(vec![-0.25_f32, -0.5])).unwrap();
        drop(tx);
        let mut dead = Command::new("true").stdin(Stdio::piped()).spawn().unwrap();
        dead.wait().unwrap();
        let mut starts = 0;
        play_queue(
            rx,
            Some(dead),
            |child| {
                if child.is_some() {
                    return Ok(());
                }
                starts += 1;
                *child = Some(
                    Command::new("cat")
                        .stdin(Stdio::piped())
                        .stdout(output.try_clone()?)
                        .spawn()?,
                );
                Ok(())
            },
            "prueba",
        )
        .unwrap();
        assert_eq!(starts, 1);
        let bytes = std::fs::read(&path).unwrap();
        let expected: Vec<_> = [0.25_f32, 0.5, -0.25, -0.5]
            .into_iter()
            .flat_map(f32::to_le_bytes)
            .collect();
        assert_eq!(bytes, expected);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn pipeline_enqueue_does_not_wait_for_playback() {
        let (tx, rx) = mpsc::channel();
        let playback = Playback {
            senders: vec![tx],
            workers: Vec::new(),
        };
        for _ in 0..100 {
            playback.enqueue(vec![0.0; crate::types::TTS_SAMPLE_RATE as usize]);
        }
        assert_eq!(rx.try_iter().count(), 100);
    }
}
