mod audio;
mod config;
mod dubbing;
mod ducking;
mod kokoro;
mod lang;
mod models;
mod oir;
mod output;
mod pipeline;
mod playback;
mod stt;
mod subtitles;
mod system_audio;
mod translate;
mod tts;
mod types;
mod vad;
mod virtual_mic;

use anyhow::Context;
use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand};
use config::{Config, TtsEngine};
use std::path::PathBuf;

const EXAMPLES: &str = "\
Ejemplos:
  voz-traductor                           Reunión: los demás oyen la traducción por \"Voz traducida\"
  voz-traductor --auriculares jam         Igual, y tú también la oyes en los cascos JAM
  voz-traductor --prueba                  Probar sin Discord: la traducción suena solo en tu salida
  voz-traductor --proveedor groq          Traducir solo con ese proveedor en esta sesión
  voz-traductor proveedores --probar      Comparar latencia y traducciones de los proveedores
  voz-traductor salidas                   Ver por dónde puedes oírte
  voz-traductor microfono estado          Ver si Discord está usando el micrófono virtual
  voz-traductor oir --app firefox         Subtítulos en español de lo que suena en Firefox
  voz-traductor oir --app firefox --doblaje   Oír el doblaje en español

En Discord: Ajustes → Voz y vídeo → Micrófono = \"Voz traducida (voz-traductor)\".
Ayuda de un comando: voz-traductor ayuda <comando>   (o <comando> --help)
Configuración: voz-traductor config";

#[derive(Parser)]
#[command(
    version,
    about = "Traduce tu voz del español al inglés y guarda el audio",
    after_help = EXAMPLES
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    #[command(flatten)]
    listen: ListenOptions,
}
#[derive(Subcommand)]
enum Command {
    /// Escuchar el micrófono (opción predeterminada)
    Escuchar(ListenOptions),
    /// Traducir un fichero WAV
    Fichero {
        /// Fichero WAV de entrada
        wav: PathBuf,
        #[command(flatten)]
        options: CommonOptions,
    },
    /// Listar micrófonos disponibles
    Dispositivos,
    /// Listar salidas de audio disponibles
    Salidas,
    /// Listar voces disponibles
    Voces {
        /// Prefijo del idioma de las voces
        #[arg(long, default_value = "en")]
        idioma: String,
        /// Motor de síntesis (predeterminado: el configurado)
        #[arg(long, value_enum)]
        motor: Option<EngineArg>,
    },
    /// Listar los proveedores de traducción configurados
    Proveedores {
        /// Traducir 3 frases de prueba con cada uno (latencia mediana)
        #[arg(long)]
        probar: bool,
    },
    /// Instalar, quitar o consultar el micrófono virtual
    Microfono {
        #[command(subcommand)]
        action: MicrophoneCommand,
    },
    /// Subtítulos en español de lo que suena en el PC
    #[command(long_about = "Subtítulos en español de lo que suena en el PC.\n\n\
        Ejemplos:\n  \
        voz-traductor oir                   Todo lo que suena, subtitulado\n  \
        voz-traductor oir --app discord     Solo el audio de Discord\n  \
        voz-traductor oir --app firefox --arriba --tamano 36\n  \
        voz-traductor oir --app firefox --doblaje --auriculares jam\n  \
        voz-traductor oir --app discord --doblaje --sin-subtitulos --sin-bajar")]
    Oir(OirOptions),
    /// Mostrar la ruta y la configuración efectiva
    Config,
}
#[derive(Subcommand)]
enum MicrophoneCommand {
    /// Instalar el micrófono virtual ahora y al iniciar PipeWire
    Instalar,
    /// Quitar los módulos y su configuración permanente
    Quitar,
    /// Mostrar instalación y aplicaciones que graban del micrófono
    Estado,
}
#[derive(Clone, Copy, clap::ValueEnum)]
enum EngineArg {
    Kokoro,
    Edge,
}
impl From<EngineArg> for TtsEngine {
    fn from(value: EngineArg) -> Self {
        match value {
            EngineArg::Kokoro => Self::Kokoro,
            EngineArg::Edge => Self::Edge,
        }
    }
}
fn apply_engine(engine: EngineArg, cfg: &mut config::TtsConfig) {
    let engine = TtsEngine::from(engine);
    if engine != cfg.engine {
        cfg.engine = engine;
        cfg.voice = match engine {
            TtsEngine::Kokoro => "bm_george",
            TtsEngine::Edge => "en-US-AndrewNeural",
        }
        .into();
    }
}
#[derive(Args, Default)]
struct CommonOptions {
    /// Oír también la traducción por la salida predeterminada
    #[arg(long, conflicts_with = "prueba")]
    oirme: bool,
    /// Nombre o descripción de los auriculares (implica --oirme)
    #[arg(long)]
    auriculares: Option<String>,
    /// Solo auriculares; no crear ni usar el micrófono virtual
    #[arg(long)]
    prueba: bool,
    /// Motor de síntesis
    #[arg(long, value_enum)]
    motor: Option<EngineArg>,
    /// Velocidad de síntesis (0,5 a 2)
    #[arg(long)]
    velocidad: Option<f32>,
    /// Voz del motor de síntesis
    #[arg(long)]
    voz: Option<String>,
    /// Carpeta de las sesiones
    #[arg(long)]
    salida: Option<PathBuf>,
    /// Modelo del traductor
    #[arg(long)]
    modelo: Option<String>,
    /// Proveedor de traducción (solo ese, sin reservas)
    #[arg(long)]
    proveedor: Option<String>,
}
#[derive(Args, Default)]
struct ListenOptions {
    /// Nombre o parte del nombre del micrófono
    #[arg(long)]
    dispositivo: Option<String>,
    #[command(flatten)]
    common: CommonOptions,
}
#[derive(Args)]
struct OirOptions {
    /// Reproducir la traducción en español (requiere --app)
    #[arg(long)]
    doblaje: bool,
    /// Voz española de Kokoro o Edge
    #[arg(long)]
    voz_es: Option<String>,
    /// Salida de doblaje por nombre o descripción
    #[arg(long)]
    auriculares: Option<String>,
    /// Conservar el volumen del original durante el doblaje
    #[arg(long)]
    sin_bajar: bool,
    /// Ocultar la ventana y los eventos de subtítulos
    #[arg(long)]
    sin_subtitulos: bool,
    /// Aplicación a escuchar (parte del nombre: discord, firefox…). Sin esto, todo el PC
    #[arg(long)]
    app: Option<String>,
    /// Mostrar también el original en inglés sobre la traducción
    #[arg(long)]
    original: bool,
    /// Subtítulos arriba en vez de abajo
    #[arg(long)]
    arriba: bool,
    /// Tamaño de letra de la traducción
    #[arg(long)]
    tamano: Option<f32>,
    /// Proveedor de traducción (solo ese, sin reservas)
    #[arg(long)]
    proveedor: Option<String>,
    /// Carpeta de las sesiones
    #[arg(long)]
    salida: Option<PathBuf>,
    /// Volcar los subtítulos a consola en vez de abrir la ventana (oculta)
    #[arg(long, hide = true)]
    sin_ventana: bool,
}
impl OirOptions {
    fn apply(self, cfg: &mut Config) -> anyhow::Result<()> {
        cfg.oir.dub |= self.doblaje;
        if let Some(voice) = self.voz_es {
            cfg.oir.dub_voice = Some(voice);
        }
        if let Some(device) = self.auriculares {
            cfg.playback.monitor_device = Some(device);
        }
        if self.sin_bajar {
            cfg.oir.duck_original = false;
        }
        if self.sin_subtitulos {
            cfg.oir.show_subtitles = false;
        }
        if let Some(app) = self.app {
            cfg.oir.app = Some(app);
        }
        if self.original {
            cfg.subtitles.show_original = true;
        }
        if self.arriba {
            cfg.subtitles.position = config::SubtitlePosition::Top;
        }
        if let Some(size) = self.tamano {
            cfg.subtitles.font_size = size;
        }
        if let Some(name) = self.proveedor {
            select_provider(cfg, &name)?;
        }
        if let Some(dir) = self.salida {
            cfg.output.dir = dir;
        }
        cfg.oir.validate_dubbing()
    }
}
fn select_provider(cfg: &mut Config, name: &str) -> anyhow::Result<()> {
    if !cfg.translate.providers.iter().any(|p| p.name == name) {
        let names = cfg
            .translate
            .providers
            .iter()
            .map(|p| p.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(anyhow::anyhow!(
            "proveedor desconocido: {name} (disponibles: {names})"
        ));
    }
    cfg.translate.providers.retain(|p| p.name == name);
    Ok(())
}
impl CommonOptions {
    fn apply(self, cfg: &mut Config) -> anyhow::Result<()> {
        if self.prueba {
            cfg.playback.virtual_mic = false;
            cfg.playback.monitor = true;
        } else if self.oirme {
            cfg.playback.monitor = true;
        }
        if let Some(device) = self.auriculares {
            cfg.playback.monitor = true;
            cfg.playback.monitor_device = Some(device);
        }
        if let Some(engine) = self.motor {
            apply_engine(engine, &mut cfg.tts);
        }
        if let Some(speed) = self.velocidad {
            cfg.tts.speed = speed;
        }
        if let Some(voice) = self.voz {
            cfg.tts.voice = voice;
        }
        if let Some(dir) = self.salida {
            cfg.output.dir = dir;
        }
        if let Some(model) = self.modelo
            && let Some(first) = cfg.translate.providers.first_mut()
        {
            first.model = model;
        }
        if let Some(name) = self.proveedor {
            select_provider(cfg, &name)?;
        }
        Ok(())
    }
}
fn probe_providers(cfg: &config::TranslateConfig) -> anyhow::Result<()> {
    const PHRASES: [&str; 3] = [
        "Hola, ¿cómo estás?",
        "Vamos a probar la traducción.",
        "La reunión empieza en cinco minutos.",
    ];
    let mut failed = 0;
    for p in &cfg.providers {
        let mut single = cfg.clone();
        single.providers = vec![p.clone()];
        let mut translator = match translate::Translator::new(&single) {
            Ok(translator) => translator,
            Err(error) => {
                println!("{}: sin clave ({error:#})", p.name);
                failed += 1;
                continue;
            }
        };
        let mut times = Vec::new();
        let mut texts = Vec::new();
        let mut ok = true;
        for phrase in PHRASES {
            let started = std::time::Instant::now();
            match translator.translate(phrase, &[]) {
                Ok(t) => {
                    times.push(started.elapsed());
                    texts.push(t.text);
                }
                Err(error) => {
                    println!("{}: error al probar ({error:#})", p.name);
                    ok = false;
                    break;
                }
            }
        }
        if !ok || times.is_empty() {
            failed += 1;
            continue;
        }
        times.sort();
        println!(
            "{} ({}): mediana {} s",
            p.name,
            p.model,
            seconds(times[times.len() / 2])
        );
        for (src, dst) in PHRASES.iter().zip(texts.iter()) {
            println!("  «{src}» → «{dst}»");
        }
    }
    if failed >= cfg.providers.len() {
        return Err(anyhow::anyhow!(
            "ningún proveedor tradujo las frases de prueba"
        ));
    }
    Ok(())
}

fn seconds(duration: std::time::Duration) -> String {
    format!("{:.2}", duration.as_secs_f64()).replace('.', ",")
}

/// `voz-traductor ayuda [comando]` equivale a `voz-traductor [comando] --help`.
fn help_alias(args: impl Iterator<Item = std::ffi::OsString>) -> Vec<std::ffi::OsString> {
    let mut args: Vec<_> = args.collect();
    if args.get(1).is_some_and(|arg| arg == "ayuda") {
        args.remove(1);
        args.push("--help".into());
    }
    args
}

fn spanish_help(mut command: clap::Command) -> clap::Command {
    command = command
        .disable_help_subcommand(true)
        .disable_help_flag(true)
        .disable_version_flag(true)
        .subcommand_help_heading("Comandos")
        .help_template("{about-with-newline}\nUso: {usage}\n\n{all-args}{after-help}")
        .arg(
            clap::Arg::new("help")
                .short('h')
                .long("help")
                .action(clap::ArgAction::Help)
                .help("Mostrar ayuda"),
        );
    if command.get_version().is_some() {
        command = command.arg(
            clap::Arg::new("version")
                .short('V')
                .long("version")
                .action(clap::ArgAction::Version)
                .help("Mostrar versión"),
        );
    }
    let headings: Vec<_> = command
        .get_arguments()
        .map(|arg| {
            (
                arg.get_id().clone(),
                if arg.is_positional() {
                    "Argumentos"
                } else {
                    "Opciones"
                },
            )
        })
        .collect();
    for (id, heading) in headings {
        command = command.mut_arg(id, |arg| arg.help_heading(heading));
    }
    for child in command.get_subcommands_mut() {
        *child = spanish_help(child.clone());
    }
    command
}
fn main() -> anyhow::Result<()> {
    let cli = Cli::from_arg_matches(
        &spanish_help(Cli::command()).get_matches_from(help_alias(std::env::args_os())),
    )?;
    match cli.command {
        Some(Command::Microfono { action }) => match action {
            MicrophoneCommand::Instalar => virtual_mic::install()?,
            MicrophoneCommand::Quitar => virtual_mic::remove()?,
            MicrophoneCommand::Estado => virtual_mic::status()?,
        },
        Some(Command::Salidas) => playback::list_outputs()?,
        Some(Command::Dispositivos) => {
            for (name, default) in audio::list_input_devices()? {
                println!("{name}{}", if default { " (predeterminado)" } else { "" });
            }
        }
        Some(Command::Voces { idioma, motor }) => {
            let path = Config::path();
            let mut cfg = if path.exists() {
                toml::from_str::<Config>(
                    &std::fs::read_to_string(&path)
                        .with_context(|| format!("No se puede leer {}", path.display()))?,
                )
                .with_context(|| format!("TOML no válido en {}", path.display()))?
                .tts
            } else {
                config::TtsConfig::default()
            };
            if let Some(engine) = motor {
                apply_engine(engine, &mut cfg);
            }
            cfg.validate()?;
            for (name, gender, locale) in tts::list_voices(&cfg, &idioma)? {
                println!("{name} · {gender} · {locale}");
            }
        }
        Some(Command::Config) => {
            let mut cfg = Config::load()?;
            cfg.translate.api_key = Some(config::mask_key(cfg.translate.api_key.as_deref()));
            cfg.translate.masked_keys();
            println!("Configuración: {}\n{cfg:#?}", Config::path().display());
        }
        Some(Command::Proveedores { probar }) => {
            let cfg = Config::load()?;
            if probar {
                probe_providers(&cfg.translate)?;
            } else {
                for p in &cfg.translate.providers {
                    println!(
                        "{} · {} · {} · {}",
                        p.name,
                        p.model,
                        p.base_url,
                        config::mask_key(p.api_key.as_deref())
                    );
                }
            }
        }
        Some(Command::Fichero { wav, options }) => {
            let mut cfg = Config::load()?;
            options.apply(&mut cfg)?;
            cfg.validate()?;
            pipeline::run(&cfg, pipeline::Source::Wav(wav))?;
        }
        Some(Command::Oir(options)) => {
            let sin_ventana = options.sin_ventana;
            let mut cfg = Config::load()?;
            options.apply(&mut cfg)?;
            cfg.validate()?;
            oir::run(&cfg, sin_ventana)?;
        }
        command => {
            let options = match command {
                Some(Command::Escuchar(options)) => options,
                _ => cli.listen,
            };
            let mut cfg = Config::load()?;
            cfg.audio.device = options.dispositivo.or(cfg.audio.device);
            options.common.apply(&mut cfg)?;
            cfg.validate()?;
            pipeline::run(&cfg, pipeline::Source::Microphone)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod cli_tests {
    use super::*;
    #[test]
    fn headphones_imply_monitor_in_every_session_mode() {
        for args in [
            vec!["voz-traductor", "--auriculares", "jam"],
            vec!["voz-traductor", "escuchar", "--auriculares", "jam"],
            vec![
                "voz-traductor",
                "fichero",
                "fixture.wav",
                "--auriculares",
                "jam",
            ],
            vec![
                "voz-traductor",
                "fichero",
                "fixture.wav",
                "--prueba",
                "--auriculares",
                "jam",
            ],
        ] {
            let cli = Cli::try_parse_from(&args).unwrap();
            let options = match cli.command {
                Some(Command::Escuchar(options)) => options.common,
                Some(Command::Fichero { options, .. }) => options,
                _ => cli.listen.common,
            };
            let mut cfg = Config::default();
            options.apply(&mut cfg).unwrap();
            assert!(cfg.playback.monitor);
            assert_eq!(cfg.playback.monitor_device.as_deref(), Some("jam"));
            assert_eq!(cfg.playback.virtual_mic, !args.contains(&"--prueba"));
        }
        assert!(matches!(
            Cli::try_parse_from(["voz-traductor", "salidas"])
                .unwrap()
                .command,
            Some(Command::Salidas)
        ));
    }
}
