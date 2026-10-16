//! Volúmenes del audio del PC: compensación de la captura y atenuación reversible.
//!
//! - `VolumeGuard` mantiene la ganancia del *source-output* propio para que la captura no
//!   dependa del volumen del stream grabado (`parecord --monitor-stream` captura después del
//!   volumen del sink-input, con curva cúbica) y guarda en disco las atenuaciones que podrían
//!   quedar recordadas para otra sesión (`ducking.json`).
//! - `Ducker` baja el volumen de los streams mientras suena el doblaje y lo restaura después.
//!
//! Ambos comparten el mismo `VolumeGuard` (un `Arc<Mutex<..>>`): la captura lo refresca cada
//! ~1 s y el `Ducker` en cada tick, de modo que la compensación sigue la rampa de atenuación
//! sin dejar la señal del VAD a 1/64.

use crate::system_audio;
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

/// Ganancia máxima de compensación: equivale a un stream al 5 % (curva cúbica de Pulse).
pub const MAX_CAPTURE_GAIN: f64 = 20.0;
/// Atenuación máxima a la que se recorta la captura si el stream suena muy alto.
const MIN_CAPTURE_GAIN: f64 = 0.05;
/// Tolerancia para reconocer un volumen que dejamos bajado (±1 % del rango).
pub const DUCK_MATCH_TOLERANCE: u32 = 656;

/// Ruta del registro de atenuaciones pendientes (`~/.local/state/voz-traductor/ducking.json`).
pub fn registry_path() -> PathBuf {
    dirs::state_dir()
        .or_else(dirs::data_dir)
        .unwrap_or_default()
        .join("voz-traductor/ducking.json")
}

type SharedGuard = Arc<Mutex<VolumeGuard>>;

/// Volumen del *source-output* propio que deja la captura a ganancia unidad, por canal.
/// El segundo valor indica que hubo que recortar (stream al 5 % o menos).
pub fn unity_gain(volumes: &[u32]) -> (f64, bool) {
    if volumes.is_empty() {
        return (1.0, false);
    }
    let cube = |volume: u32| f64::from(volume).powi(3);
    let mean =
        (volumes.iter().map(|volume| cube(*volume)).sum::<f64>() / volumes.len() as f64).cbrt();
    if mean <= 0.0 {
        return (MAX_CAPTURE_GAIN, true);
    }
    let gain = 65536.0 / mean;
    if gain > MAX_CAPTURE_GAIN {
        (MAX_CAPTURE_GAIN, true)
    } else {
        (gain.max(MIN_CAPTURE_GAIN), false)
    }
}

/// Decisión de compensación para un stream: volumen del grabador propio y aviso (si procede).
#[derive(Debug, PartialEq, Eq)]
pub struct GainPlan {
    pub volumes: Vec<u32>,
    pub warning: Option<String>,
}

/// Calcula la compensación y, si el stream está casi en silencio o muteado, el aviso.
pub fn gain_plan(volumes: &[u32], muted: bool, name: &str) -> GainPlan {
    let (gain, limited) = unity_gain(volumes);
    let target = (65536.0 * gain).round() as u32;
    let warning = if muted {
        Some(format!(
            "El audio de {name} está en silencio (mute); actívalo para poder traducirlo"
        ))
    } else if limited {
        Some(format!(
            "El audio de {name} está casi en silencio; súbelo para poder traducirlo"
        ))
    } else {
        None
    };
    GainPlan {
        volumes: vec![target],
        warning,
    }
}

/// Atenuación aplicada a una app: valor original y valor al que la bajamos.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct DuckEntry {
    app_name: String,
    binary: String,
    original: Vec<u32>,
    ducked: Vec<u32>,
}

impl DuckEntry {
    fn same_app(&self, app_name: &str, binary: &str) -> bool {
        self.app_name.eq_ignore_ascii_case(app_name) && self.binary.eq_ignore_ascii_case(binary)
    }

    /// El volumen del stream es el que dejamos bajado (±1 %).
    fn matches(&self, volumes: &[u32]) -> bool {
        volumes.len() == self.ducked.len()
            && volumes
                .iter()
                .zip(&self.ducked)
                .all(|(volume, ducked)| volume.abs_diff(*ducked) <= DUCK_MATCH_TOLERANCE)
    }

    fn name(&self) -> &str {
        if self.app_name.trim().is_empty() {
            &self.binary
        } else {
            &self.app_name
        }
    }
}

/// Estado compartido entre la captura y el doblaje.
pub struct VolumeGuard {
    path: PathBuf,
    entries: Vec<DuckEntry>,
    /// Streams que el `Ducker` vivo está atenuando ahora mismo: la captura no los toca.
    owned: HashSet<u32>,
    /// Volumen aplicado al source-output propio de cada stream (para no repetir `pactl`).
    applied: HashMap<u32, Vec<u32>>,
    /// Apps ya avisadas de su estado actual (se rearma al corregirse).
    warned: HashSet<String>,
}

/// Clave estable de una app para no repetir avisos si cambia el stream.
fn app_key(app_name: &str, binary: &str) -> String {
    format!("{}\u{1}{binary}", app_name.to_lowercase())
}

impl VolumeGuard {
    /// Carga el registro pendiente; si el fichero no se entiende, avisa y empieza vacío.
    pub fn load(path: PathBuf) -> Self {
        let entries = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| match serde_json::from_str::<Vec<DuckEntry>>(&text) {
                Ok(entries) => Some(entries),
                Err(error) => {
                    eprintln!(
                        "Aviso: no se entiende el registro de volúmenes {} ({error}); se ignora",
                        path.display()
                    );
                    None
                }
            })
            .unwrap_or_default();
        Self {
            path,
            entries,
            owned: HashSet::new(),
            applied: HashMap::new(),
            warned: HashSet::new(),
        }
    }

    /// Escribe el registro de forma atómica (temporal + rename) y sin fichero si está vacío.
    fn save(&self) -> Result<()> {
        if self.entries.is_empty() {
            std::fs::remove_file(&self.path).ok();
            return Ok(());
        }
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let temporary = self.path.with_extension("json.tmp");
        let mut file = File::create(&temporary)?;
        file.write_all(&serde_json::to_vec_pretty(&self.entries)?)?;
        file.sync_data()?;
        drop(file);
        std::fs::rename(&temporary, &self.path)?;
        Ok(())
    }

    /// Registra la atenuación **antes** de bajar el volumen y marca el stream como propio.
    pub fn register(
        &mut self,
        stream: u32,
        app_name: &str,
        binary: &str,
        original: &[u32],
        ducked: &[u32],
    ) -> Result<()> {
        self.own(stream);
        self.entries
            .retain(|entry| !entry.same_app(app_name, binary));
        self.entries.push(DuckEntry {
            app_name: app_name.to_owned(),
            binary: binary.to_owned(),
            original: original.to_vec(),
            ducked: ducked.to_vec(),
        });
        self.save()
    }

    /// Actualiza el valor bajado tras cada escritura de la rampa (deja el registro consistente
    /// aunque el proceso muera a medias).
    pub fn settle(&mut self, app_name: &str, binary: &str, ducked: &[u32]) -> Result<()> {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.same_app(app_name, binary))
            && entry.ducked != ducked
        {
            entry.ducked = ducked.to_vec();
            return self.save();
        }
        Ok(())
    }

    /// Quita la entrada (el volumen ya volvió a su sitio).
    pub fn clear(&mut self, app_name: &str, binary: &str) -> Result<()> {
        let before = self.entries.len();
        self.entries
            .retain(|entry| !entry.same_app(app_name, binary));
        if self.entries.len() != before {
            self.save()?;
        }
        Ok(())
    }

    pub fn own(&mut self, stream: u32) {
        self.owned.insert(stream);
    }

    pub fn disown(&mut self, stream: u32) {
        self.owned.remove(&stream);
    }

    /// Si el stream es de una app con atenuación pendiente y su volumen es el que dejamos
    /// bajado, lo devuelve al original, avisa y borra la entrada. Nada si no coincide.
    pub fn restore_pending(
        &mut self,
        stream: u32,
        app_name: &str,
        binary: &str,
        volumes: &[u32],
    ) -> Result<bool> {
        if self.owned.contains(&stream) {
            return Ok(false);
        }
        let Some(entry) = self
            .entries
            .iter()
            .find(|entry| entry.same_app(app_name, binary) && entry.matches(volumes))
            .cloned()
        else {
            return Ok(false);
        };
        set_volume("set-sink-input-volume", stream, &entry.original)?;
        self.entries
            .retain(|other| !other.same_app(app_name, binary));
        self.save()?;
        println!("Restaurado el volumen de {} que quedó bajado", entry.name());
        Ok(true)
    }

    /// Compensa en el source-output propio el volumen del sink-input capturado y avisa
    /// (una vez por app) si el stream está casi en silencio o muteado.
    pub fn capture_gain(
        &mut self,
        stream: u32,
        volumes: &[u32],
        muted: bool,
        app_name: &str,
        binary: &str,
    ) -> Result<()> {
        let plan = gain_plan(volumes, muted, app_name);
        if self.applied.get(&stream) != Some(&plan.volumes) {
            self.apply_capture_gain(stream, &plan.volumes)?;
        }
        let key = app_key(app_name, binary);
        if self.note_warning(&key, plan.warning.as_deref()) {
            eprintln!("{}", plan.warning.unwrap_or_default());
        }
        Ok(())
    }

    /// Marca el aviso de una app y devuelve `true` solo la primera vez; cuando deja de
    /// estarlo se rearma.
    fn note_warning(&mut self, key: &str, warning: Option<&str>) -> bool {
        match warning {
            Some(_) => self.warned.insert(key.to_owned()),
            None => {
                self.warned.remove(key);
                false
            }
        }
    }

    fn apply_capture_gain(&mut self, stream: u32, volumes: &[u32]) -> Result<()> {
        let Some(found) = find_source_output(stream)? else {
            // El grabador propio todavía no existe; se reintenta en el siguiente refresco.
            return Ok(());
        };
        let value = volumes.first().copied().unwrap_or(65536);
        let desired = vec![value; found.volumes.len().max(1)];
        set_volume("set-source-output-volume", found.id, &desired)?;
        self.applied.insert(stream, desired);
        Ok(())
    }

    /// Olvida el estado de un stream que ya no se captura.
    pub fn release(&mut self, stream: u32) {
        self.applied.remove(&stream);
    }
}

fn set_volume(kind: &str, id: u32, volumes: &[u32]) -> Result<()> {
    let result = Command::new("pactl")
        .args([kind, &id.to_string()])
        .args(volumes.iter().map(u32::to_string))
        .output()?;
    ensure!(
        result.status.success(),
        "No se pudo ajustar el volumen del stream {id}"
    );
    Ok(())
}

struct SourceOutput {
    id: u32,
    volumes: Vec<u32>,
}

/// El source-output propio (`parecord --monitor-stream`) del stream indicado, si existe.
fn find_source_output(stream: u32) -> Result<Option<SourceOutput>> {
    let result = Command::new("pactl")
        .args(["-f", "json", "list", "source-outputs"])
        .output()?;
    ensure!(
        result.status.success(),
        "No se puede consultar el grabador propio"
    );
    let items: Vec<Value> = serde_json::from_slice(&result.stdout)?;
    let name = system_audio::capture_stream_name(&stream.to_string());
    items
        .iter()
        .find(|item| {
            item["properties"]["media.name"].as_str() == Some(&name)
                && item["properties"]["application.name"].as_str() == Some(&name)
        })
        .map(|item| {
            Ok(SourceOutput {
                id: item["index"]
                    .as_u64()
                    .context("Grabador propio sin índice")? as u32,
                volumes: system_audio::channel_volumes(item)?,
            })
        })
        .transpose()
}

#[derive(Clone)]
struct Snapshot {
    id: u32,
    identity: String,
    app_name: String,
    binary: String,
    muted: bool,
    volumes: Vec<u32>,
}
impl Snapshot {
    fn parse(value: &Value) -> Result<Self> {
        let id = value["index"].as_u64().context("Stream sin índice")? as u32;
        let identity = format!(
            "{}:{}:{}:{}",
            value["client"],
            value["properties"]["object.serial"],
            value["properties"]["application.process.id"],
            value["properties"]["application.name"]
        );
        Ok(Self {
            id,
            identity,
            app_name: value["properties"]["application.name"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            binary: value["properties"]["application.process.binary"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            muted: value["mute"].as_bool().unwrap_or(false),
            volumes: system_audio::channel_volumes(value)?,
        })
    }
}
struct OwnedVolume {
    snapshot: Snapshot,
    last: Vec<u32>,
    from: Vec<u32>,
    target: Vec<u32>,
    ramp_started: Instant,
}
impl OwnedVolume {
    fn still_owned(&self, current: &Snapshot) -> bool {
        current.identity == self.snapshot.identity && current.volumes == self.last
    }
}
fn snapshots(app: &str) -> Result<Vec<Snapshot>> {
    system_audio::captured_streams(app)?
        .iter()
        .map(Snapshot::parse)
        .collect()
}

pub struct Ducker {
    app: String,
    factor: f32,
    guard: SharedGuard,
    owned: HashMap<u32, OwnedVolume>,
    manual: HashSet<(u32, String)>,
    backup: File,
    last_voice: Option<Instant>,
    last_poll: Option<Instant>,
}
impl Ducker {
    pub fn new(app: &str, factor: f32, path: &Path, guard: SharedGuard) -> Result<Self> {
        ensure!(
            (0.0..=1.0).contains(&factor) && factor > 0.0,
            "Factor de atenuación no válido"
        );
        Ok(Self {
            app: app.into(),
            factor,
            guard,
            owned: HashMap::new(),
            manual: HashSet::new(),
            backup: File::create(path)?,
            last_voice: None,
            last_poll: None,
        })
    }
    fn lock_guard(&self) -> std::sync::MutexGuard<'_, VolumeGuard> {
        self.guard.lock().unwrap_or_else(|error| error.into_inner())
    }
    pub fn tick(&mut self, playing: bool) -> Result<()> {
        let now = Instant::now();
        if playing {
            self.last_voice = Some(now);
        }
        if self
            .last_poll
            .is_some_and(|last| now.duration_since(last) < Duration::from_millis(30))
        {
            return Ok(());
        }
        self.last_poll = Some(now);
        let active = playing
            || self
                .last_voice
                .is_some_and(|last| now.duration_since(last) < Duration::from_millis(300));
        let current = snapshots(&self.app)?;
        let gone: Vec<_> = self
            .owned
            .keys()
            .copied()
            .filter(|id| !current.iter().any(|item| item.id == *id))
            .collect();
        for id in gone {
            // El stream desapareció con el volumen bajado: la entrada queda pendiente
            // para restaurarla cuando la app vuelva a sonar.
            self.lock_guard().disown(id);
            self.owned.remove(&id);
        }
        self.manual.retain(|(id, identity)| {
            current
                .iter()
                .any(|item| item.id == *id && item.identity == *identity)
        });
        for item in current {
            if self.manual.contains(&(item.id, item.identity.clone())) {
                continue;
            }
            if let Some(owned) = self.owned.get(&item.id) {
                if !owned.still_owned(&item) {
                    eprintln!(
                        "Volumen del stream {} cambiado externamente; se respeta el ajuste manual",
                        item.id
                    );
                    self.manual.insert((item.id, item.identity.clone()));
                    set_volume("set-sink-input-volume", item.id, &owned.snapshot.volumes)?;
                    let (name, binary) = (
                        owned.snapshot.app_name.clone(),
                        owned.snapshot.binary.clone(),
                    );
                    {
                        let mut guard = self.lock_guard();
                        guard.clear(&name, &binary)?;
                        guard.disown(item.id);
                    }
                    self.owned.remove(&item.id);
                    continue;
                }
            } else if active {
                // Copia durable ANTES de la primera escritura, y registro para no dejar
                // el volumen recordado si el stream desaparece.
                writeln!(
                    self.backup,
                    "{}",
                    serde_json::json!({"stream":item.id,"identity":item.identity,"volumes":item.volumes})
                )?;
                self.backup.flush()?;
                self.backup.sync_data()?;
                let target = scaled(&item.volumes, self.factor);
                self.lock_guard().register(
                    item.id,
                    &item.app_name,
                    &item.binary,
                    &item.volumes,
                    &target,
                )?;
                self.owned.insert(
                    item.id,
                    OwnedVolume {
                        snapshot: item.clone(),
                        last: item.volumes.clone(),
                        from: item.volumes.clone(),
                        target,
                        ramp_started: now,
                    },
                );
            }
            let mut change = None;
            if let Some(owned) = self.owned.get_mut(&item.id) {
                let target = if active {
                    scaled(&owned.snapshot.volumes, self.factor)
                } else {
                    owned.snapshot.volumes.clone()
                };
                if target != owned.target {
                    owned.from = owned.last.clone();
                    owned.target = target;
                    owned.ramp_started = now;
                }
                let progress =
                    (now.duration_since(owned.ramp_started).as_secs_f32() / 0.08).min(1.0);
                let next = interpolate(&owned.from, &owned.target, progress);
                if next != owned.last {
                    change = Some(next);
                }
            }
            if let Some(next) = change {
                set_volume("set-sink-input-volume", item.id, &next)?;
                if let Some(owned) = self.owned.get_mut(&item.id) {
                    owned.last = next.clone();
                }
                let mut guard = self.lock_guard();
                guard.settle(&item.app_name, &item.binary, &next)?;
                guard.capture_gain(item.id, &next, item.muted, &item.app_name, &item.binary)?;
            } else if let Some(owned) = self.owned.get(&item.id) {
                // Sin cambio esta vuelta: la compensación sigue la rampa sin esperar al
                // refresco de la captura (~1 s).
                let current = owned.last.clone();
                self.lock_guard().capture_gain(
                    item.id,
                    &current,
                    item.muted,
                    &item.app_name,
                    &item.binary,
                )?;
            }
        }
        Ok(())
    }
}
// Pulse aplica una curva cúbica: .25 en su mando implica 1/64 en PCM.
fn scaled(volumes: &[u32], factor: f32) -> Vec<u32> {
    volumes
        .iter()
        .map(|volume| {
            if *volume == 0 {
                0
            } else {
                ((f64::from(*volume) * f64::from(factor)).round() as u32).max(1)
            }
        })
        .collect()
}
fn interpolate(from: &[u32], to: &[u32], progress: f32) -> Vec<u32> {
    from.iter()
        .zip(to)
        .map(|(a, b)| {
            (f64::from(*a) + (f64::from(*b) - f64::from(*a)) * f64::from(progress)).round() as u32
        })
        .collect()
}
impl Drop for Ducker {
    fn drop(&mut self) {
        // Restauración inmediata incluso al fallar, sin esperar la rampa/cola de voz.
        match snapshots(&self.app) {
            Ok(current) => {
                for item in current {
                    let Some(owned) = self.owned.get(&item.id) else {
                        continue;
                    };
                    let (name, binary) = (
                        owned.snapshot.app_name.clone(),
                        owned.snapshot.binary.clone(),
                    );
                    if owned.still_owned(&item)
                        && let Err(error) =
                            set_volume("set-sink-input-volume", item.id, &owned.snapshot.volumes)
                    {
                        eprintln!("Error al restaurar stream {}: {error:#}", item.id);
                        continue;
                    }
                    {
                        let mut guard = self.lock_guard();
                        if let Err(error) = guard.clear(&name, &binary) {
                            eprintln!("Error al borrar el registro de volumen: {error:#}");
                        }
                        guard.disown(item.id);
                    }
                }
            }
            Err(error) => eprintln!("Error al consultar volúmenes al restaurar: {error:#}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guard_en_temp() -> (VolumeGuard, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "voz-traductor-guard-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ducking.json");
        (VolumeGuard::load(path.clone()), path)
    }

    #[test]
    fn channel_order_scaling_and_manual_override() {
        let snapshot = Snapshot::parse(&serde_json::json!({"index":7,"client":"9","channel_map":"front-right,front-left","volume":{"front-left":{"value":49151},"front-right":{"value":23457}}})).unwrap();
        assert_eq!(snapshot.volumes, [23457, 49151]);
        assert_eq!(scaled(&snapshot.volumes, 0.25), [5864, 12288]);
        assert_eq!(
            interpolate(&[1, 60000], &snapshot.volumes, 1.0),
            snapshot.volumes
        );
        let owned = OwnedVolume {
            last: vec![5864, 12288],
            from: vec![],
            target: vec![],
            ramp_started: Instant::now(),
            snapshot: snapshot.clone(),
        };
        let mut current = snapshot;
        current.volumes = owned.last.clone();
        assert!(owned.still_owned(&current));
        current.volumes[0] += 1;
        assert!(!owned.still_owned(&current));
        current.volumes = owned.last.clone();
        current.identity.push('x');
        assert!(!owned.still_owned(&current));
    }

    #[test]
    fn compensacion_a_ganancia_unidad_con_tope() {
        assert_eq!(unity_gain(&[65536]), (1.0, false));
        // 25 % en el mando son 1/64 de amplitud: se compensa ×4.
        assert_eq!(unity_gain(&[16384]), (4.0, false));
        // 5 % es el tope.
        let (gain, limited) = unity_gain(&[3277]);
        assert!(gain > 19.9 && gain <= 20.0, "{gain}");
        assert!(!limited);
        let (gain, limited) = unity_gain(&[3276]);
        assert_eq!(gain, 20.0);
        assert!(limited);
        let (gain, limited) = unity_gain(&[0]);
        assert_eq!(gain, 20.0);
        assert!(limited);
        // Por encima del 100 % se recorta la captura sin avisar.
        assert_eq!(unity_gain(&[131072]), (0.5, false));
        // Canales distintos: media cúbica por canal.
        let (gain, _) = unity_gain(&[65536, 32768]);
        assert!((gain - 1.2114).abs() < 0.001, "{gain}");
    }

    #[test]
    fn aviso_solo_si_esta_casi_en_silencio_o_muteado() {
        assert_eq!(gain_plan(&[65536], false, "Firefox").warning, None);
        assert_eq!(gain_plan(&[16384], false, "Firefox").warning, None);
        assert_eq!(gain_plan(&[16384], false, "Firefox").volumes, vec![262144]);
        let plan = gain_plan(&[1000], false, "Firefox");
        assert_eq!(plan.volumes, vec![1310720]);
        assert_eq!(
            plan.warning.as_deref(),
            Some("El audio de Firefox está casi en silencio; súbelo para poder traducirlo")
        );
        let muted = gain_plan(&[65536], true, "Discord");
        assert_eq!(
            muted.warning.as_deref(),
            Some("El audio de Discord está en silencio (mute); actívalo para poder traducirlo")
        );
    }

    #[test]
    fn registro_persistente_de_atenuaciones() {
        let (mut guard, path) = guard_en_temp();
        let original = vec![42137, 51789];
        let ducked = scaled(&original, 0.25);
        guard
            .register(7, "Firefox", "firefox", &original, &ducked)
            .unwrap();
        // Escrito antes de bajar: otra carga lo ve.
        let recargado = VolumeGuard::load(path.clone());
        assert_eq!(recargado.entries.len(), 1);
        assert_eq!(recargado.entries[0].original, original);
        assert!(recargado.entries[0].matches(&ducked));
        // La rampa actualiza el valor bajado y sigue siendo consistente.
        let mid = interpolate(&original, &ducked, 0.5);
        guard.settle("Firefox", "firefox", &mid).unwrap();
        assert!(VolumeGuard::load(path.clone()).entries[0].matches(&mid));
        // Al restaurar, se borra (y el fichero desaparece).
        guard.clear("Firefox", "firefox").unwrap();
        assert!(VolumeGuard::load(path.clone()).entries.is_empty());
        assert!(!path.exists());
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn coincidencia_por_identidad_de_app_con_tolerancia() {
        let entry = DuckEntry {
            app_name: "Firefox".into(),
            binary: "firefox".into(),
            original: vec![65536],
            ducked: vec![16384],
        };
        assert!(entry.matches(&[16384]));
        assert!(entry.matches(&[16384 + DUCK_MATCH_TOLERANCE]));
        assert!(!entry.matches(&[16384 + DUCK_MATCH_TOLERANCE + 1]));
        assert!(!entry.matches(&[65536]));
        assert!(!entry.matches(&[16384, 16384]));
        assert!(entry.same_app("firefox", "Firefox"));
        assert!(!entry.same_app("Chromium", "firefox"));
        assert!(!entry.same_app("Firefox", "chrome"));
    }

    #[test]
    fn solo_toca_las_apps_del_fichero() {
        let (mut guard, path) = guard_en_temp();
        guard
            .register(7, "Firefox", "firefox", &[65536], &[16384])
            .unwrap();
        // Otra app con el mismo volumen no se toca (no devuelve restauración).
        assert!(
            !guard
                .restore_pending(9, "Chromium", "chrome", &[16384])
                .unwrap()
        );
        // Un stream del Ducker vivo tampoco (lo restaura él).
        guard.own(7);
        assert!(
            !guard
                .restore_pending(7, "Firefox", "firefox", &[16384])
                .unwrap()
        );
        guard.disown(7);
        // Stream de la app del fichero pero con otro volumen: tampoco.
        assert!(
            !guard
                .restore_pending(8, "Firefox", "firefox", &[65536])
                .unwrap()
        );
        assert_eq!(VolumeGuard::load(path.clone()).entries.len(), 1);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn aviso_una_vez_por_app_y_se_rearma() {
        let (mut guard, path) = guard_en_temp();
        let key = app_key("Firefox", "firefox");
        assert!(guard.note_warning(&key, Some("casi en silencio")));
        assert!(!guard.note_warning(&key, Some("casi en silencio")));
        assert!(!guard.note_warning(&key, None));
        assert!(guard.note_warning(&key, Some("casi en silencio")));
        // Otra app se avisa por su cuenta.
        assert!(guard.note_warning(&app_key("Discord", "discord"), Some("en silencio")));
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }
}
