//! Comando `oir`: subtítulos en español de lo que suena en el PC.
//!
//! Captura → VAD → STT (inglés) → filtro de idioma → traducción → ventana de
//! subtítulos en el hilo principal. En disco queda `<fecha>-oir/` con
//! `transcripcion.md` y `subtitulos.srt` (sin audio).

use crate::{
    config::Config,
    dubbing::{DubStats, Dubbing},
    ducking,
    lang::{EchoGuard, TargetFilter},
    stt::Stt,
    subtitles::{SubtitleEvent, run_window},
    system_audio,
    translate::Translator,
    types::{Timings, Utterance},
    vad::Segmenter,
};
use anyhow::{Context, Result, anyhow};
use std::{
    fs::File,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

/// Frase lista para la sesión, la consola y (vía eventos) la ventana.
struct OirPhrase {
    id: u32,
    source_text: String,
    target_text: String,
    provider: String,
    timings: Timings,
    /// Instante en que empezó a sonar (para el SRT, relativo al inicio).
    utter_start: Instant,
    utter_dur: Duration,
}

fn seconds(duration: Duration) -> String {
    format!("{:.2}", duration.as_secs_f64()).replace('.', ",")
}

fn print_phrase(phrase: &OirPhrase, terminal: bool) {
    if terminal {
        print!("\r\x1b[2K");
    }
    println!(
        "#{}  en  {}\n    es  {}\n        audio {} s · stt {} s · trad {} s · {} · fin→sub {} s",
        phrase.id,
        phrase.source_text,
        phrase.target_text,
        seconds(phrase.utter_dur),
        seconds(phrase.timings.stt),
        seconds(phrase.timings.translate),
        phrase.provider,
        seconds(phrase.timings.total)
    );
}

fn srt_time(d: Duration) -> String {
    let ms = d.as_millis();
    format!(
        "{:02}:{:02}:{:02},{:03}",
        ms / 3_600_000,
        ms / 60_000 % 60,
        ms / 1_000 % 60,
        ms % 1_000
    )
}

struct SrtEntry {
    start: Duration,
    end: Duration,
    text: String,
}

/// Sesión `<fecha>-oir/`: `transcripcion.md` incremental + `subtitulos.srt`
/// (tiempos relativos al inicio). Sin audio.
struct OirSession {
    dir: PathBuf,
    md: File,
    srt_path: PathBuf,
    started: Instant,
    timings: Vec<Timings>,
    providers: Vec<String>,
    entries: Vec<SrtEntry>,
}

impl OirSession {
    fn create(dir: &Path, cfg: &Config) -> Result<Self> {
        std::fs::create_dir_all(dir)
            .map_err(|e| anyhow!("no se pudo crear {}: {}", dir.display(), e))?;
        let dir = std::path::absolute(dir).map_err(|e| anyhow!("ruta no válida: {e}"))?;
        let base_ts = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
        let mut session_dir = dir.join(format!("{base_ts}-oir"));
        let mut n = 1;
        while session_dir.exists() {
            n += 1;
            session_dir = dir.join(format!("{base_ts}-oir-{n}"));
        }
        std::fs::create_dir_all(&session_dir)
            .map_err(|e| anyhow!("no se pudo crear {}: {}", session_dir.display(), e))?;
        let started = chrono::Local::now();
        let header = format!(
            "# Subtítulos — {}\n\n- Fecha: {}\n- Modelo de traducción: {}\n\n\
             | # | English | Español | Proveedor | STT | Trad. | Total |\n\
             |---|---|---|---|---|---|---|\n",
            started.format("%Y-%m-%d %H:%M:%S"),
            started.format("%Y-%m-%d %H:%M:%S"),
            cfg.translate.describe(),
        );
        let mut md = File::create(session_dir.join("transcripcion.md"))
            .map_err(|e| anyhow!("no se pudo crear transcripcion.md: {e}"))?;
        md.write_all(header.as_bytes())
            .map_err(|e| anyhow!("no se pudo escribir la cabecera: {e}"))?;
        md.flush()
            .map_err(|e| anyhow!("no se pudo volcar transcripcion.md: {e}"))?;
        let srt_path = session_dir.join("subtitulos.srt");
        File::create(&srt_path).map_err(|e| anyhow!("no se pudo crear subtitulos.srt: {e}"))?;
        Ok(Self {
            dir: session_dir,
            md,
            srt_path,
            started: Instant::now(),
            timings: Vec::new(),
            providers: Vec::new(),
            entries: Vec::new(),
        })
    }

    fn esc_cell(s: &str) -> String {
        s.replace('|', "\\|")
            .replace("\r\n", " ")
            .replace(['\r', '\n'], " ")
            .trim()
            .to_owned()
    }

    fn write(&mut self, phrase: &OirPhrase) -> Result<()> {
        let row = format!(
            "| {} | {} | {} | {} | {} s | {} s | {} s |\n",
            phrase.id,
            Self::esc_cell(&phrase.source_text),
            Self::esc_cell(&phrase.target_text),
            Self::esc_cell(&phrase.provider),
            seconds(phrase.timings.stt),
            seconds(phrase.timings.translate),
            seconds(phrase.timings.total),
        );
        self.md
            .write_all(row.as_bytes())
            .map_err(|e| anyhow!("no se pudo escribir la frase {}: {e}", phrase.id))?;
        self.md
            .flush()
            .map_err(|e| anyhow!("no se pudo volcar transcripcion.md: {e}"))?;

        let start = phrase.utter_start.saturating_duration_since(self.started);
        let entry = SrtEntry {
            start,
            end: start + phrase.utter_dur,
            text: phrase.target_text.clone(),
        };
        let n = self.entries.len() + 1;
        let mut srt = File::options()
            .append(true)
            .open(&self.srt_path)
            .map_err(|e| anyhow!("no se pudo añadir a subtitulos.srt: {e}"))?;
        write!(
            srt,
            "{n}\n{} --> {}\n{}\n\n",
            srt_time(entry.start),
            srt_time(entry.end),
            entry.text
        )
        .map_err(|e| anyhow!("no se pudo escribir subtitulos.srt: {e}"))?;
        srt.flush()
            .map_err(|e| anyhow!("no se pudo volcar subtitulos.srt: {e}"))?;
        self.entries.push(entry);

        self.timings.push(phrase.timings.clone());
        self.providers.push(phrase.provider.clone());
        Ok(())
    }

    fn finish(mut self) -> Result<PathBuf> {
        // Recortar solapes (el fin no pasa del inicio de la siguiente).
        for i in 0..self.entries.len().saturating_sub(1) {
            if self.entries[i].end > self.entries[i + 1].start {
                self.entries[i].end = self.entries[i + 1].start;
            }
        }
        let mut srt = File::create(&self.srt_path)
            .map_err(|e| anyhow!("no se pudo cerrar subtitulos.srt: {e}"))?;
        for (i, entry) in self.entries.iter().enumerate() {
            write!(
                srt,
                "{}\n{} --> {}\n{}\n\n",
                i + 1,
                srt_time(entry.start),
                srt_time(entry.end),
                entry.text
            )
            .map_err(|e| anyhow!("no se pudo cerrar subtitulos.srt: {e}"))?;
        }
        srt.flush()
            .map_err(|e| anyhow!("no se pudo volcar subtitulos.srt: {e}"))?;

        let n = self.timings.len();
        writeln!(self.md, "\n## Resumen\n")
            .map_err(|e| anyhow!("no se pudo cerrar la transcripción: {e}"))?;
        writeln!(self.md, "- Frases: {n}")
            .map_err(|e| anyhow!("no se pudo cerrar la transcripción: {e}"))?;
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
                .map_err(|e| anyhow!("no se pudo cerrar la transcripción: {e}"))?;
            let stt: Vec<f64> = self.timings.iter().map(|t| t.stt.as_secs_f64()).collect();
            let tra: Vec<f64> = self
                .timings
                .iter()
                .map(|t| t.translate.as_secs_f64())
                .collect();
            let tot: Vec<f64> = self.timings.iter().map(|t| t.total.as_secs_f64()).collect();
            for (label, v) in [("STT", stt), ("Traducción", tra), ("Total", tot)] {
                writeln!(
                    self.md,
                    "- {label}: media {} · p50 {} · máx {}",
                    fmt_secs(mean(&v)),
                    fmt_secs(percentile(&v, 50.0)),
                    fmt_secs(max(&v))
                )
                .map_err(|e| anyhow!("no se pudo cerrar la transcripción: {e}"))?;
            }
        }
        self.md
            .flush()
            .map_err(|e| anyhow!("no se pudo volcar transcripcion.md: {e}"))?;
        Ok(self.dir)
    }

    fn dubbing_summary(&mut self, stats: &DubStats) -> Result<()> {
        let speeds: Vec<f64> = stats.speeds.iter().map(|speed| f64::from(*speed)).collect();
        writeln!(
            self.md,
            "\n## Doblaje\n\n- Cola media: {:.3} s\n- Cola máxima: {:.3} s\n- Velocidad media: {:.3}\n- Velocidad máxima: {:.3}\n- Trozos reproducidos: {}\n- Descartados por cola: {}\n- Cancelados al parar: {}\n",
            stats.queue_mean,
            stats.queue_max,
            mean(&speeds),
            max(&speeds),
            stats.played,
            stats.discarded,
            stats.cancelled
        )?;
        self.md.flush()?;
        std::fs::write(
            self.dir.join("doblaje-resumen.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "queue_mean_seconds":stats.queue_mean,"queue_max_seconds":stats.queue_max,"speeds":stats.speeds,
                "played":stats.played,"discarded":stats.discarded,"cancelled":stats.cancelled
            }))?,
        )?;
        Ok(())
    }

    fn discard(self) -> Result<()> {
        anyhow::ensure!(
            self.timings.is_empty(),
            "No se puede descartar una sesión con frases"
        );
        let dir = self.dir.clone();
        drop(self);
        std::fs::remove_dir_all(&dir)
            .map_err(|error| anyhow!("No se pudo descartar {}: {error}", dir.display()))
    }

    fn path(&self) -> &Path {
        &self.dir
    }
}

fn fmt_secs(secs: f64) -> String {
    format!("{:.2}", secs).replace('.', ",") + " s"
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

fn join_worker<T>(handle: thread::ScopedJoinHandle<'_, Result<T>>) -> Result<T> {
    handle
        .join()
        .map_err(|_| anyhow!("Una etapa de oír terminó inesperadamente"))?
}

/// Ejecuta el comando `oir`. Con `no_window`, vuelca los eventos a consola en
/// vez de abrir la ventana (diagnóstico y pruebas sin pantalla).
pub fn run(cfg: &Config, no_window: bool) -> Result<Option<PathBuf>> {
    cfg.oir.validate_dubbing()?;
    let no_window = no_window || !cfg.oir.show_subtitles;
    let stop = Arc::new(AtomicBool::new(false));
    let signal_stop = stop.clone();
    ctrlc::set_handler(move || {
        // No salir desde el handler: Drop debe restaurar los volúmenes también
        // tras SIGTERM y señales repetidas.
        signal_stop.store(true, Ordering::SeqCst);
    })
    .context("No se puede instalar el manejador de Ctrl+C")?;
    let terminal = io::stdout().is_terminal();
    let session = OirSession::create(&cfg.output.dir, cfg)?;
    let (event_tx, event_rx) = mpsc::channel::<SubtitleEvent>();
    match cfg.oir.app.as_deref() {
        Some(app) => println!("Escuchando el audio de «{app}» (inglés → español)"),
        None => println!("Escuchando todo lo que suena en el PC (inglés → español)"),
    }
    println!(
        "Traductor: {}\nSesión: {}",
        cfg.translate.describe(),
        session.path().display()
    );
    if !cfg.oir.show_subtitles {
        println!("Subtítulos ocultos");
    } else if no_window {
        println!("Sin ventana: los subtítulos salen por esta consola");
    } else {
        println!("Cierra la ventana o pulsa Ctrl+C para terminar");
    }

    let result = thread::scope(|scope| -> Result<Option<PathBuf>> {
        let worker_tx = event_tx.clone();
        let worker_stop = stop.clone();
        let worker =
            scope.spawn(move || oir_worker(cfg, session, worker_tx, &worker_stop, terminal));
        // Sin remitentes en este hilo, al terminar el pipeline el canal se
        // cierra: vuelve la ventana (también tras Ctrl+C, vía `stop`) y acaba
        // el volcado a consola.
        drop(event_tx);
        if no_window {
            // Toda la orquesta corre aquí; los eventos se vuelcan a consola.
            for event in event_rx {
                match event {
                    SubtitleEvent::Line {
                        id,
                        original,
                        translated,
                    } => {
                        if cfg.oir.show_subtitles {
                            println!("[subtítulo #{id}] {original} → {translated}");
                        }
                    }
                    SubtitleEvent::Speaking(speaking) => {
                        println!("[hablando: {speaking}]");
                    }
                    SubtitleEvent::Status(text) => {
                        println!("[estado] {text}");
                    }
                    SubtitleEvent::Quit => break,
                }
                if stop.load(Ordering::Relaxed) {
                    break;
                }
            }
            return worker
                .join()
                .map_err(|_| anyhow!("Una etapa de oír terminó inesperadamente"))?;
        }
        // La ventana manda en el hilo principal; el pipeline vive en el otro.
        let window = run_window(&cfg.subtitles, event_rx);
        stop.store(true, Ordering::SeqCst);
        let path = worker
            .join()
            .map_err(|_| anyhow!("Una etapa de oír terminó inesperadamente"))??;
        window?;
        Ok(path)
    });
    if terminal {
        print!("\r\x1b[2K");
    }
    result
}

struct Transcribed {
    utterance: Utterance,
    text: String,
    timings: Timings,
}

#[allow(clippy::too_many_lines)]
fn oir_worker(
    cfg: &Config,
    mut session: OirSession,
    event_tx: mpsc::Sender<SubtitleEvent>,
    stop: &Arc<AtomicBool>,
    terminal: bool,
) -> Result<Option<PathBuf>> {
    let mut vad_cfg = cfg.vad.clone();
    vad_cfg.silence_ms = cfg.oir.silence_ms;
    vad_cfg.max_speech_ms = cfg.oir.max_speech_ms;
    let mut tcfg = cfg.translate.clone();
    tcfg.source_lang = cfg.oir.source_lang.clone();
    tcfg.target_lang = cfg.oir.target_lang.clone();

    let mut segmenter = if cfg.oir.chunking {
        Segmenter::new_chunked(
            &vad_cfg,
            cfg.oir.min_chunk_ms,
            cfg.oir.chunk_pause_ms,
            cfg.oir.max_chunk_ms,
        )?
    } else {
        Segmenter::new(&vad_cfg)?
    };
    // Estado compartido: la captura compensa el volumen del stream y el doblaje lo
    // atenúa, sin pelearse ni dejar volúmenes recordados.
    let capture_guard = Arc::new(Mutex::new(ducking::VolumeGuard::load(
        ducking::registry_path(),
    )));
    let dubbing = if cfg.oir.dub {
        event_tx
            .send(SubtitleEvent::Status(
                "Preparando doblaje en español…".into(),
            ))
            .ok();
        Some(Dubbing::start(
            cfg,
            session.path(),
            stop.clone(),
            capture_guard.clone(),
        )?)
    } else {
        None
    };
    let (frame_tx, frame_rx) = mpsc::sync_channel::<Vec<f32>>(32);
    let (utterance_tx, utterance_rx) = mpsc::sync_channel::<Utterance>(4);
    let (text_tx, text_rx) = mpsc::sync_channel::<Transcribed>(4);
    let (phrase_tx, phrase_rx) = mpsc::sync_channel::<OirPhrase>(4);
    let (ready_tx, ready_rx) = mpsc::channel::<std::result::Result<(), String>>();
    let mut received_phrase = false;
    let echo = EchoGuard::default();
    let filter = cfg
        .stt
        .skip_target_language
        .then(|| TargetFilter::new(&cfg.oir.source_lang, &cfg.oir.target_lang))
        .flatten();
    let (echo, filter) = (&echo, filter.as_ref());
    let event_status = event_tx.clone();
    let quit_tx = event_tx.clone();
    let result = thread::scope(|scope| -> Result<()> {
        let vad = scope.spawn(move || -> Result<()> {
            let mut speaking = false;
            let send = |utterance: Utterance| {
                if !terminal {
                    println!(
                        "[trozo #{}] audio {:.3} s; retrospectiva {:.3} s",
                        utterance.id,
                        utterance.duration().as_secs_f64(),
                        utterance.ended_at.elapsed().as_secs_f64()
                    );
                }
                utterance_tx.send(utterance)
            };
            for frame in frame_rx {
                match segmenter.push_frame(&frame) {
                    Ok(done) => {
                        if speaking != segmenter.is_speaking() {
                            speaking = segmenter.is_speaking();
                            if terminal {
                                print!("\r\x1b[2K{}", if speaking { "● hablando…" } else { "" });
                                io::stdout().flush().ok();
                            }
                            event_tx.send(SubtitleEvent::Speaking(speaking)).ok();
                        }
                        if let Some(utterance) = done
                            && send(utterance).is_err()
                        {
                            return Ok(());
                        }
                    }
                    Err(error) => eprintln!("Error de detección de voz: {error:#}"),
                }
            }
            if let Some(utterance) = segmenter.flush() {
                send(utterance).ok();
            }
            Ok(())
        });
        let stt_ready = ready_tx.clone();
        let stt = scope.spawn(move || -> Result<()> {
            let started = Instant::now();
            let mut stt = match Stt::load(&cfg.stt) {
                Ok(stt) => {
                    stt_ready.send(Ok(())).ok();
                    stt
                }
                Err(error) => {
                    stt_ready.send(Err(format!("{error:#}"))).ok();
                    return Err(error);
                }
            };
            drop(stt_ready);
            println!("Modelo STT cargado en {} s", seconds(started.elapsed()));
            for utterance in utterance_rx {
                let started = Instant::now();
                match stt.transcribe(&utterance.samples) {
                    Ok(text) if text.chars().any(char::is_alphanumeric) => {
                        if let Some(filter) = filter
                            && !filter.is_source(&text)
                            && (echo.is_echo(&text) || filter.is_target(&text))
                        {
                            if terminal {
                                print!("\r\x1b[2K\x1b[2m");
                            }
                            print!("    (ignorado, ya está en español o es eco: {text})");
                            println!("{}", if terminal { "\x1b[0m" } else { "" });
                            continue;
                        }
                        let timings = Timings {
                            stt: started.elapsed(),
                            ..Timings::default()
                        };
                        if text_tx
                            .send(Transcribed {
                                utterance,
                                text,
                                timings,
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(error) => {
                        eprintln!(
                            "Frase #{} (audio de {} s): error de transcripción: {error:#}",
                            utterance.id,
                            seconds(utterance.duration())
                        )
                    }
                }
            }
            Ok(())
        });
        let translate_ready = ready_tx.clone();
        let translator = scope.spawn(move || -> Result<()> {
            let mut translator = match Translator::for_subtitles(&tcfg) {
                Ok(translator) => {
                    translate_ready.send(Ok(())).ok();
                    translator
                }
                Err(error) => {
                    translate_ready.send(Err(format!("{error:#}"))).ok();
                    return Err(error);
                }
            };
            drop(translate_ready);
            let mut context = Vec::new();
            for mut transcribed in text_rx {
                let started = Instant::now();
                match translator.translate(&transcribed.text, &context) {
                    Ok(t) if !t.text.trim().is_empty() => {
                        echo.remember(&t.text);
                        transcribed.timings.translate = started.elapsed();
                        transcribed.timings.total = transcribed.utterance.ended_at.elapsed();
                        if tcfg.context_phrases > 0 {
                            context.push((transcribed.text.clone(), t.text.clone()));
                            if context.len() > tcfg.context_phrases {
                                context.remove(0);
                            }
                        }
                        let utter_dur = transcribed.utterance.duration();
                        let utter_start = transcribed
                            .utterance
                            .ended_at
                            .checked_sub(utter_dur)
                            .unwrap_or(transcribed.utterance.ended_at);
                        if phrase_tx
                            .send(OirPhrase {
                                id: transcribed.utterance.id,
                                source_text: transcribed.text,
                                target_text: t.text,
                                provider: t.provider,
                                timings: transcribed.timings,
                                utter_start,
                                utter_dur,
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(error) => eprintln!(
                        "Frase #{}: error de traducción: {error:#}",
                        transcribed.utterance.id
                    ),
                }
            }
            Ok(())
        });
        let mut startup_error = None;
        for _ in 0..2 {
            if let Err(error) = ready_rx.recv().context("No se pudo iniciar una etapa")? {
                startup_error = Some(error);
            }
        }
        let source_stop = stop.clone();
        let app = cfg.oir.app.clone();
        let event_capture = event_status.clone();
        let source_worker = scope.spawn(move || -> Result<()> {
            if let Some(error) = startup_error {
                drop(frame_tx);
                return Err(anyhow!(error));
            }
            system_audio::capture_system_audio(
                app.as_deref(),
                frame_tx,
                &source_stop,
                &|text| {
                    event_capture.send(SubtitleEvent::Status(text)).ok();
                },
                &capture_guard,
            )
        });
        for phrase in phrase_rx {
            received_phrase = true;
            if let Err(error) = session.write(&phrase) {
                eprintln!("Frase #{}: error al guardar: {error:#}", phrase.id);
            }
            print_phrase(&phrase, terminal);
            if let Some(dubbing) = &dubbing {
                dubbing.enqueue(phrase.id, &phrase.target_text);
            }
            event_status
                .send(SubtitleEvent::Line {
                    id: phrase.id,
                    original: phrase.source_text.clone(),
                    translated: phrase.target_text.clone(),
                })
                .ok();
        }
        let results = [
            join_worker(source_worker),
            join_worker(vad),
            join_worker(stt),
            join_worker(translator),
        ];
        for result in results {
            result?;
        }
        Ok(())
    });
    let dub_result = if let Some(dubbing) = dubbing {
        let (stats, result) = dubbing.finish();
        session.dubbing_summary(&stats)?;
        result
    } else {
        Ok(())
    };
    // El pipeline terminó (parada o error): la ventana se cierra sola.
    quit_tx.send(SubtitleEvent::Quit).ok();
    let path = if received_phrase {
        let path = session.finish()?;
        println!("Sesión guardada en {}", path.display());
        Some(path)
    } else {
        session.discard()?;
        println!("No se detectó ninguna frase; no se ha guardado sesión.");
        None
    };
    result?;
    dub_result?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subtitles::SubtitleEvent;

    #[test]
    fn protocolo_de_eventos_en_orden() {
        let (tx, rx) = mpsc::channel();
        tx.send(SubtitleEvent::Speaking(true)).unwrap();
        tx.send(SubtitleEvent::Status("probando".into())).unwrap();
        tx.send(SubtitleEvent::Line {
            id: 1,
            original: "Hello".into(),
            translated: "Hola".into(),
        })
        .unwrap();
        tx.send(SubtitleEvent::Speaking(false)).unwrap();
        tx.send(SubtitleEvent::Quit).unwrap();
        drop(tx);
        let events: Vec<_> = rx.iter().collect();
        assert_eq!(events.len(), 5);
        assert!(matches!(events[4], SubtitleEvent::Quit));
    }

    #[test]
    fn srt_formatea_horas_minutos_y_milis() {
        assert_eq!(srt_time(Duration::from_millis(3_200)), "00:00:03,200");
        assert_eq!(srt_time(Duration::from_millis(61_500)), "00:01:01,500");
        assert_eq!(
            srt_time(Duration::from_millis(3_600_000 + 120_000 + 5_000 + 42)),
            "01:02:05,042"
        );
    }
}
