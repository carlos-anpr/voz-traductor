//! Configuración: valores por defecto < fichero TOML < variables de entorno < flags de CLI.
//! Fichero: `~/.config/voz-traductor/config.toml` (todas las claves opcionales).

use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub audio: AudioConfig,
    pub vad: VadConfig,
    pub stt: SttConfig,
    pub translate: TranslateConfig,
    pub tts: TtsConfig,
    pub output: OutputConfig,
    pub playback: PlaybackConfig,
    pub oir: OirConfig,
    pub subtitles: SubtitlesConfig,
}

/// Sentido inverso (`voz-traductor oir`): traducir lo que suena en el PC.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct OirConfig {
    /// Aplicación a escuchar (parte del nombre: "discord", "firefox"…). `None` = todo lo que suena.
    pub app: Option<String>,
    pub source_lang: String,
    pub target_lang: String,
    /// Pausa que cierra una frase: más corta que en el micro porque el habla grabada apenas tiene pausas.
    pub silence_ms: u32,
    /// Corte forzado para no esperar a pausas que no llegan (podcasts).
    pub max_speech_ms: u32,
    /// Troceo de baja latencia solo para `oir`; false recupera las frases completas.
    pub chunking: bool,
    /// Esperar al menos este tiempo antes de buscar una pausa corta.
    pub min_chunk_ms: u32,
    pub chunk_pause_ms: u32,
    /// Límite de espera; sin pausa se busca un valle en los últimos 800 ms.
    pub max_chunk_ms: u32,
    pub dub: bool,
    /// None selecciona em_alex en Kokoro o es-ES-AlvaroNeural en Edge.
    pub dub_voice: Option<String>,
    pub duck_original: bool,
    pub duck_volume: f32,
    pub max_speed: f32,
    pub show_subtitles: bool,
}

impl Default for OirConfig {
    fn default() -> Self {
        Self {
            app: None,
            source_lang: "English".into(),
            target_lang: "Spanish".into(),
            silence_ms: 450,
            max_speech_ms: 8_000,
            chunking: true,
            min_chunk_ms: 1_500,
            chunk_pause_ms: 200,
            max_chunk_ms: 3_500,
            dub: false,
            dub_voice: None,
            duck_original: true,
            duck_volume: 0.25,
            max_speed: 1.35,
            show_subtitles: true,
        }
    }
}

impl OirConfig {
    pub fn validate_dubbing(&self) -> anyhow::Result<()> {
        if !self.dub {
            return Ok(());
        }
        anyhow::ensure!(
            self.app
                .as_deref()
                .is_some_and(|app| !app.trim().is_empty()),
            "El doblaje requiere --app para excluir su propia reproducción y evitar bucles"
        );
        anyhow::ensure!(
            (0.0..=1.0).contains(&self.duck_volume) && self.duck_volume > 0.0,
            "oir.duck_volume debe ser mayor que 0 y como máximo 1 (la captura necesita señal)"
        );
        anyhow::ensure!(
            (1.0..=2.0).contains(&self.max_speed),
            "oir.max_speed debe estar entre 1 y 2"
        );
        anyhow::ensure!(
            self.dub_voice
                .as_ref()
                .is_none_or(|voice| !voice.trim().is_empty()),
            "La voz de doblaje no puede estar vacía"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SubtitlePosition {
    Top,
    Bottom,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SubtitlesConfig {
    pub position: SubtitlePosition,
    /// Tamaño de letra del texto traducido, en puntos lógicos.
    pub font_size: f32,
    /// Mostrar también el texto original, más pequeño, encima de la traducción.
    pub show_original: bool,
    /// Frases visibles a la vez (la más reciente abajo).
    pub max_lines: usize,
    /// Segundos que permanece una frase si no llega otra.
    pub hide_after_secs: f32,
    /// Ancho máximo de la caja respecto al ancho de la pantalla.
    pub width_fraction: f32,
    /// Opacidad del fondo (0 = transparente, 1 = opaco).
    pub background_opacity: f32,
}

impl Default for SubtitlesConfig {
    fn default() -> Self {
        Self {
            position: SubtitlePosition::Bottom,
            font_size: 30.0,
            show_original: false,
            max_lines: 2,
            hide_after_secs: 8.0,
            width_fraction: 0.8,
            background_opacity: 0.8,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct PlaybackConfig {
    /// Enviar la traducción al micrófono virtual (lo que oyen en Discord).
    pub virtual_mic: bool,
    /// Reproducir también la traducción en tus auriculares.
    pub monitor: bool,
    /// Salida para oírte (nombre o parte de la descripción, p. ej. "JAM"). `None` = salida predeterminada.
    pub monitor_device: Option<String>,
}

impl Default for PlaybackConfig {
    fn default() -> Self {
        Self { virtual_mic: true, monitor: false, monitor_device: None }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct AudioConfig {
    /// Nombre (o parte del nombre) del dispositivo de entrada. `None` = el predeterminado del sistema.
    pub device: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct VadConfig {
    /// Probabilidad mínima de Silero para considerar voz.
    pub threshold: f32,
    /// Silencio que cierra una frase.
    pub silence_ms: u32,
    /// Audio previo que se conserva al detectar el inicio de voz.
    pub preroll_ms: u32,
    /// Frases más cortas se descartan (toses, clics).
    pub min_speech_ms: u32,
    /// Corte forzado de frases demasiado largas.
    pub max_speech_ms: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SttConfig {
    /// Carpeta del modelo Parakeet TDT 0.6b v3 int8 (por defecto, la que ya descargó Handy).
    pub model_dir: PathBuf,
    /// Descarta las frases transcritas que ya están en el idioma de destino: evita el eco de la propia
    /// traducción (altavoces) y no traduce lo que dicen los demás en inglés.
    pub skip_target_language: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct TranslateConfig {
    pub source_lang: String,
    pub target_lang: String,
    /// Frases anteriores (pares origen→destino) que se envían como contexto.
    pub context_phrases: usize,
    /// Orden = prioridad. El primero es el principal. Vacío = se materializa
    /// uno con los campos antiguos de `[translate]` (compatibilidad).
    #[serde(default)]
    pub providers: Vec<TranslateProvider>,
    // Campos antiguos: solo compatibilidad con ficheros sin `providers`.
    // Tras `load()`, `providers` nunca está vacío y estos dejan de usarse.
    pub base_url: String,
    pub model: String,
    pub api_key: Option<String>,
    pub extra_body: serde_json::Value,
    pub timeout_secs: u64,
}

/// Un proveedor de traducción (`[[translate.providers]]`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct TranslateProvider {
    /// Nombre corto (sale en consola y en `transcripcion.md`).
    pub name: String,
    /// Base OpenAI-compatible, sin `/chat/completions`.
    pub base_url: String,
    pub model: String,
    /// Clave directa (o `api_key_env` con el nombre de la variable).
    /// Si no hay clave y la URL es de opencode.ai, se usa la de
    /// `~/.local/share/opencode/auth.json`.
    pub api_key: Option<String>,
    pub api_key_env: Option<String>,
    /// Campos extra que se fusionan en el cuerpo de la petición.
    pub extra_body: serde_json::Value,
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct TtsConfig {
    pub engine: TtsEngine,
    /// Voz de Kokoro (`bm_george`, `am_michael`…) o de Edge (`en-US-AndrewNeural`…), según `engine`.
    pub voice: String,
    /// Velocidad: 1.0 = normal (Kokoro); en Edge se traduce a porcentaje.
    pub speed: f32,
    /// Carpeta con `kokoro-v1.0.fp16.onnx` y `voices-v1.0.bin` (se descargan si faltan).
    pub kokoro_dir: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TtsEngine {
    /// Local, ~0,5 s por frase.
    Kokoro,
    /// Servicio de Microsoft Edge, ~3-5 s por frase.
    Edge,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct OutputConfig {
    /// Carpeta donde se crea una subcarpeta por sesión.
    pub dir: PathBuf,
}

impl Default for VadConfig {
    fn default() -> Self {
        Self { threshold: 0.5, silence_ms: 900, preroll_ms: 300, min_speech_ms: 400, max_speech_ms: 15_000 }
    }
}

impl Default for SttConfig {
    fn default() -> Self {
        let home = dirs::home_dir().unwrap_or_default();
        Self {
            model_dir: home.join(".local/share/com.pais.handy/models/parakeet-tdt-0.6b-v3-int8"),
            skip_target_language: true,
        }
    }
}

impl Default for TranslateConfig {
    /// Sin proveedor por defecto: cada usuario configura el suyo en `config.toml`.
    fn default() -> Self {
        Self {
            base_url: String::new(),
            model: String::new(),
            api_key: None,
            source_lang: "Spanish".into(),
            target_lang: "English".into(),
            extra_body: serde_json::Value::Null,
            context_phrases: 3,
            timeout_secs: 20,
            providers: Vec::new(),
        }
    }
}

impl TranslateConfig {
    /// `nombre (modelo)` de cada proveedor, unidos con ` + `.
    pub fn describe(&self) -> String {
        self.providers
            .iter()
            .map(|p| format!("{} ({})", p.name, p.model))
            .collect::<Vec<_>>()
            .join(" + ")
    }

    /// Sustituye cada clave por su versión enmascarada (para `config`/`proveedores`).
    pub fn masked_keys(&mut self) {
        for p in &mut self.providers {
            p.api_key = Some(mask_key(p.api_key.as_deref()));
        }
    }
}

/// Nombre corto para un proveedor heredado del formato antiguo, desde su URL
/// (`https://api.groq.com/openai/v1` → `groq`).
fn short_name_from_url(url: &str) -> String {
    let host = url.split("://").nth(1).unwrap_or(url);
    let host = host.split('/').next().unwrap_or("");
    let host = host.strip_prefix("api.").unwrap_or(host);
    let first = host.split('.').next().unwrap_or("").trim();
    if first.is_empty() {
        "principal".into()
    } else {
        first.into()
    }
}

/// Resuelve la clave de un proveedor de la lista: `api_key` > `api_key_env` >
/// `VOZ_API_KEY` > `auth.json` de OpenCode (solo hosts de opencode.ai).
fn resolve_provider_key(p: &mut TranslateProvider) {
    normalize_key(&mut p.api_key);
    if p.api_key.is_none()
        && let Some(env_name) = p.api_key_env.as_deref()
        && let Ok(v) = std::env::var(env_name)
    {
        let trimmed = v.trim().to_owned();
        if !trimmed.is_empty() {
            p.api_key = Some(trimmed);
        }
    }
    if p.api_key.is_none()
        && let Ok(env_key) = std::env::var("VOZ_API_KEY")
    {
        let trimmed = env_key.trim().to_owned();
        if !trimmed.is_empty() {
            p.api_key = Some(trimmed);
        }
    }
    if p.api_key.is_none()
        && p.base_url.contains("opencode.ai")
        && let Some(k) = read_opencode_go_key()
    {
        p.api_key = Some(k);
    }
}

impl Default for TtsConfig {
    fn default() -> Self {
        let data = dirs::data_dir().unwrap_or_default();
        Self {
            engine: TtsEngine::Kokoro,
            voice: "bm_george".into(),
            speed: 1.0,
            kokoro_dir: data.join("voz-traductor/kokoro"),
        }
    }
}

impl TtsConfig {
    /// Valida `voice` no vacía y `speed` en 0,5..2,0.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.voice.trim().is_empty() {
            return Err(anyhow::anyhow!(
                "voz vacía: indica una voz en `tts.voice` o con --voz"
            ));
        }
        if !(0.5..=2.0).contains(&self.speed) {
            return Err(anyhow::anyhow!(
                "velocidad de voz fuera de rango (0,5..2,0): {}",
                self.speed
            ));
        }
        Ok(())
    }
}

impl Default for OutputConfig {
    fn default() -> Self {
        let dir = dirs::audio_dir()
            .map(|d| d.join("voz-traductor"))
            .or_else(|| dirs::home_dir().map(|h| h.join("voz-traductor")))
            .unwrap_or_else(|| PathBuf::from("voz-traductor"));
        Self { dir }
    }
}

impl Config {
    /// Ruta del fichero de configuración del usuario.
    pub fn path() -> PathBuf {
        dirs::config_dir().unwrap_or_default().join("voz-traductor/config.toml")
    }

    /// Carga defaults + fichero (si existe), resuelve las claves y materializa
    /// los campos antiguos de `[translate]` como único proveedor si no hay `providers`.
    /// Error claro si el fichero no parsea. Sin clave en ningún proveedor no se
    /// tumba aquí: `Translator::new` avisa y salta los que no tengan (salvo que
    /// no quede ninguno).
    pub fn load() -> anyhow::Result<Self> {
        let mut cfg = Self::default();
        let path = Self::path();
        if path.exists() {
            let content = std::fs::read_to_string(&path).map_err(|e| {
                anyhow::anyhow!("no se pudo leer {}: {}", path.display(), e)
            })?;
            cfg = toml::from_str::<Self>(&content).map_err(|e| {
                anyhow::anyhow!("{} no es un TOML válido: {}", path.display(), e)
            })?;
        }
        if cfg.translate.providers.is_empty() && !cfg.translate.base_url.trim().is_empty() {
            // Formato antiguo: un proveedor con los campos de `[translate]`.
            // Semántica idéntica a la anterior: env `VOZ_API_KEY` sobrescribe,
            // y si la URL es de opencode.ai, la clave de `auth.json`.
            normalize_key(&mut cfg.translate.api_key);
            if let Ok(env_key) = std::env::var("VOZ_API_KEY") {
                let trimmed = env_key.trim().to_owned();
                if !trimmed.is_empty() {
                    cfg.translate.api_key = Some(trimmed);
                }
            }
            if cfg.translate.api_key.is_none()
                && cfg.translate.base_url.contains("opencode.ai")
                && let Some(k) = read_opencode_go_key()
            {
                cfg.translate.api_key = Some(k);
            }
            let name = short_name_from_url(&cfg.translate.base_url);
            cfg.translate.providers = vec![TranslateProvider {
                name,
                base_url: cfg.translate.base_url.clone(),
                model: cfg.translate.model.clone(),
                api_key: cfg.translate.api_key.clone(),
                api_key_env: None,
                extra_body: cfg.translate.extra_body.clone(),
                timeout_secs: cfg.translate.timeout_secs,
            }];
        } else {
            for p in &mut cfg.translate.providers {
                resolve_provider_key(p);
            }
        }
        cfg.validate()?;
        Ok(cfg)
    }

    /// Valida la síntesis y que quede algún proveedor (tras `--proveedor`).
    /// Llamar también tras aplicar los flags de CLI, que sobrescriben el fichero.
    pub fn validate(&self) -> anyhow::Result<()> {
        self.tts.validate()?;
        if self.translate.providers.is_empty() {
            return Err(anyhow::anyhow!(
                "no hay ningún proveedor de traducción. Añade al menos un \
                 `[[translate.providers]]` (cualquier API compatible con OpenAI) en {}",
                Self::path().display()
            ));
        }
        Ok(())
    }
}

fn normalize_key(key: &mut Option<String>) {
    if let Some(k) = key {
        let t = k.trim().to_owned();
        if t.is_empty() {
            *key = None;
        } else {
            *key = Some(t);
        }
    }
}

pub(crate) fn mask_key(key: Option<&str>) -> String {
    match key {
        None => "(sin clave)".to_owned(),
        Some(k) => {
            let t = k.trim();
            if t.is_empty() {
                return "(sin clave)".to_owned();
            }
            let chars: Vec<char> = t.chars().collect();
            if chars.len() <= 6 {
                return "…".to_owned();
            }
            let head: String = chars.iter().take(4).collect();
            let tail: String = chars.iter().skip(chars.len() - 2).collect();
            format!("{head}…{tail}")
        }
    }
}

fn read_opencode_go_key() -> Option<String> {
    let home = dirs::home_dir()?;
    let path = home.join(".local/share/opencode/auth.json");
    let content = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&content).ok()?;
    let key = v.get("opencode-go")?.get("key")?.as_str()?;
    let trimmed = key.trim();
    if trimmed.is_empty() { None } else { Some(trimmed.to_owned()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masked_muestra_principio_y_fin() {
        assert_eq!(mask_key(Some("sk-abcdef123456f3")), "sk-a…f3");
    }

    #[test]
    fn masked_sin_clave() {
        assert_eq!(mask_key(None), "(sin clave)");
        assert_eq!(mask_key(Some("   ")), "(sin clave)");
    }

    #[test]
    fn tts_por_defecto_valido() {
        Config::default().tts.validate().unwrap();
    }

    #[test]
    fn tts_rechaza_speed_fuera_de_rango() {
        let mut cfg = Config::default();
        cfg.tts.speed = 0.4;
        assert!(cfg.tts.validate().is_err());
        cfg.tts.speed = 2.1;
        assert!(cfg.tts.validate().is_err());
        cfg.tts.speed = 1.5;
        assert!(cfg.tts.validate().is_ok());
    }

    #[test]
    fn tts_rechaza_voz_vacia() {
        let mut cfg = Config::default();
        cfg.tts.voice = "   ".into();
        assert!(cfg.tts.validate().is_err());
    }

    #[test]
    fn formato_antiguo_da_un_proveedor_groq() {
        let cfg: Config = toml::from_str(
            "[translate]\n\
             base_url = \"https://api.groq.com/openai/v1\"\n\
             model = \"qwen/qwen3.8-27b\"\n\
             api_key = \"gsk-prueba\"\n\
             extra_body = { reasoning_effort = \"none\" }\n",
        )
        .unwrap();
        assert!(cfg.translate.providers.is_empty());
        assert_eq!(short_name_from_url(&cfg.translate.base_url), "groq");
        assert_eq!(cfg.translate.model, "qwen/qwen3.8-27b");
    }

    #[test]
    fn providers_con_prioridad_y_clave_por_env() {
        let cfg: Config = toml::from_str(
            "[translate]\n\
             context_phrases = 3\n\
             [[translate.providers]]\n\
             name = \"groq\"\n\
             base_url = \"https://api.groq.com/openai/v1\"\n\
             model = \"qwen/qwen3.8-27b\"\n\
             api_key_env = \"GROQ_API_KEY\"\n\
             timeout_secs = 3\n\
             [[translate.providers]]\n\
             name = \"opencode-go\"\n\
             base_url = \"https://opencode.ai/inference/go/openai/v1\"\n\
             model = \"deepseek-v4.1-flash\"\n\
             timeout_secs = 15\n",
        )
        .unwrap();
        assert_eq!(cfg.translate.providers.len(), 2);
        assert_eq!(cfg.translate.providers[0].name, "groq");
        assert_eq!(
            cfg.translate.providers[0].api_key_env.as_deref(),
            Some("GROQ_API_KEY")
        );
        assert_eq!(
            cfg.translate.describe(),
            "groq (qwen/qwen3.8-27b) + opencode-go (deepseek-v4.1-flash)"
        );
    }

    #[test]
    fn masked_keys_enmascara_cada_proveedor() {
        let mut cfg = Config::default();
        cfg.translate.providers.push(TranslateProvider {
            api_key: Some("sk-abcdef123456f3".into()),
            ..Default::default()
        });
        cfg.translate.masked_keys();
        assert_eq!(
            cfg.translate.providers[0].api_key.as_deref(),
            Some("sk-a…f3")
        );
    }

    #[test]
    fn sin_fichero_no_hay_proveedor_por_defecto() {
        let cfg = Config::default();
        assert!(cfg.translate.providers.is_empty());
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn playback_por_defecto_al_micro_virtual() {
        let cfg = Config::default();
        assert!(cfg.playback.virtual_mic);
        assert!(!cfg.playback.monitor);
        let cfg: Config = toml::from_str("[playback]\nmonitor = true").unwrap();
        assert!(cfg.playback.virtual_mic);
        assert!(cfg.playback.monitor);
    }
}
