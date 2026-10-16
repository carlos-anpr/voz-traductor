//! Etapas bloqueantes conectadas mediante canales acotados.
use crate::{
    audio,
    config::Config,
    lang::{EchoGuard, TargetFilter},
    output::Session,
    playback::Playback,
    stt::Stt,
    translate::Translator,
    tts::Tts,
    types::{Phrase, Timings, Utterance},
    vad::Segmenter,
};
use anyhow::{Context, Result, anyhow};
use std::{
    io::{self, IsTerminal, Write},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

pub enum Source {
    Microphone,
    Wav(PathBuf),
}
struct Transcribed {
    utterance: Utterance,
    text: String,
    timings: Timings,
}
struct Translated {
    transcribed: Transcribed,
    text: String,
    provider: String,
}

fn join_worker<T>(handle: thread::ScopedJoinHandle<'_, Result<T>>) -> Result<T> {
    handle
        .join()
        .map_err(|_| anyhow!("Una etapa de audio terminó inesperadamente"))?
}
fn seconds(duration: std::time::Duration) -> String {
    format!("{:.2}", duration.as_secs_f64()).replace('.', ",")
}
fn skipped(terminal: bool, text: &str) {
    if terminal {
        print!("\r\x1b[2K\x1b[2m");
    }
    print!("    (ignorado, ya está en el idioma de destino o es eco: {text})");
    println!("{}", if terminal { "\x1b[0m" } else { "" });
}

fn print_phrase(phrase: &Phrase, terminal: bool) {
    if terminal {
        print!("\r\x1b[2K");
    }
    println!(
        "#{}  es  {}\n    en  {}\n        stt {} s · trad {} s · {} · voz {} s · total {} s",
        phrase.id,
        phrase.source_text,
        phrase.target_text,
        seconds(phrase.timings.stt),
        seconds(phrase.timings.translate),
        phrase.provider,
        seconds(phrase.timings.tts),
        seconds(phrase.timings.total)
    );
}

pub fn run(cfg: &Config, source: Source) -> Result<Option<PathBuf>> {
    if matches!(&source, Source::Microphone) {
        audio::prepare_microphone(cfg.audio.device.as_deref())?;
    }
    let stop = Arc::new(AtomicBool::new(false));
    let signal_stop = stop.clone();
    ctrlc::set_handler(move || {
        if signal_stop.swap(true, Ordering::SeqCst) {
            std::process::exit(130);
        }
    })
    .context("No se puede instalar el manejador de Ctrl+C")?;
    let playback = Playback::start(&cfg.playback)?;
    let terminal = io::stdout().is_terminal();
    let mut session = Session::create(&cfg.output.dir, cfg)?;
    println!(
        "Traductor: {}\nVoz: {}\nSesión: {}",
        cfg.translate.describe(),
        cfg.tts.voice,
        session.path().display()
    );
    let mut segmenter = Segmenter::new(&cfg.vad)?;
    let (frame_tx, frame_rx) = mpsc::sync_channel::<Vec<f32>>(32);
    let (utterance_tx, utterance_rx) = mpsc::sync_channel::<Utterance>(4);
    let (text_tx, text_rx) = mpsc::sync_channel::<Transcribed>(4);
    let (translation_tx, translation_rx) = mpsc::sync_channel::<Translated>(4);
    let (phrase_tx, phrase_rx) = mpsc::sync_channel::<Phrase>(4);
    let (ready_tx, ready_rx) = mpsc::channel::<std::result::Result<(), String>>();
    let mut received_phrase = false;
    let echo = EchoGuard::default();
    let filter = cfg
        .stt
        .skip_target_language
        .then(|| TargetFilter::new(&cfg.translate.source_lang, &cfg.translate.target_lang))
        .flatten();
    let (echo, filter) = (&echo, filter.as_ref());
    let result = thread::scope(|scope| -> Result<()> {
        let vad = scope.spawn(move || -> Result<()> {
            let mut speaking = false;
            for frame in frame_rx {
                match segmenter.push_frame(&frame) {
                    Ok(phrase) => {
                        if terminal && speaking != segmenter.is_speaking() {
                            speaking = segmenter.is_speaking();
                            print!("\r\x1b[2K{}", if speaking { "● hablando…" } else { "" });
                            io::stdout().flush().ok();
                        }
                        if let Some(phrase) = phrase
                            && utterance_tx.send(phrase).is_err()
                        {
                            return Ok(());
                        }
                    }
                    Err(error) => eprintln!("Error de detección de voz: {error:#}"),
                }
            }
            if let Some(phrase) = segmenter.flush() {
                utterance_tx.send(phrase).ok();
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
                            skipped(terminal, &text);
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
            let mut translator = match Translator::new(&cfg.translate) {
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
                        if cfg.translate.context_phrases > 0 {
                            context.push((transcribed.text.clone(), t.text.clone()));
                            if context.len() > cfg.translate.context_phrases {
                                context.remove(0);
                            }
                        }
                        if translation_tx
                            .send(Translated {
                                transcribed,
                                text: t.text,
                                provider: t.provider,
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
        let tts = scope.spawn(move || -> Result<()> {
            let mut tts = match Tts::new(&cfg.tts) {
                Ok(tts) => {
                    ready_tx.send(Ok(())).ok();
                    tts
                }
                Err(error) => {
                    ready_tx.send(Err(format!("{error:#}"))).ok();
                    return Err(error);
                }
            };
            drop(ready_tx);
            loop {
                let translated = match translation_rx.recv_timeout(Duration::from_secs(5)) {
                    Ok(translated) => translated,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        tts.keep_warm();
                        continue;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                };
                let Transcribed {
                    utterance,
                    text,
                    mut timings,
                } = translated.transcribed;
                let started = Instant::now();
                match tts.synthesize(&translated.text) {
                    Ok(audio) => {
                        timings.tts = started.elapsed();
                        timings.total = utterance.ended_at.elapsed();
                        if phrase_tx
                            .send(Phrase {
                                id: utterance.id,
                                original: utterance.samples,
                                source_text: text,
                                target_text: translated.text,
                                provider: translated.provider,
                                audio,
                                timings,
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(error) => {
                        eprintln!("Frase #{}: error de síntesis: {error:#}", utterance.id)
                    }
                }
            }
            Ok(())
        });
        let mut startup_error = None;
        for _ in 0..3 {
            if let Err(error) = ready_rx.recv().context("No se pudo iniciar una etapa")? {
                startup_error = Some(error);
            }
        }
        let source_stop = stop.clone();
        let source_worker = scope.spawn(move || -> Result<()> {
            if let Some(error) = startup_error {
                drop(frame_tx);
                return Err(anyhow!(error));
            }
            match source {
                Source::Microphone => {
                    audio::capture_microphone(cfg.audio.device.as_deref(), frame_tx, source_stop)
                }
                Source::Wav(path) => {
                    println!("Fichero: {}", path.display());
                    let samples = audio::read_wav_16k(&path)?;
                    audio::send_frames(&samples, &frame_tx, &source_stop);
                    Ok(())
                }
            }
        });
        for phrase in phrase_rx {
            received_phrase = true;
            if let Err(error) = session.write(&phrase) {
                eprintln!("Frase #{}: error al guardar: {error:#}", phrase.id);
            }
            print_phrase(&phrase, terminal);
            playback.enqueue(phrase.audio);
        }
        // Siempre unir todas las etapas, también si una de ellas falló.
        let results = [
            join_worker(source_worker),
            join_worker(vad),
            join_worker(stt),
            join_worker(translator),
            join_worker(tts),
        ];
        for result in results {
            result?;
        }
        Ok(())
    });
    if terminal {
        print!("\r\x1b[2K");
    }
    let path = if received_phrase {
        let path = session.finish()?;
        println!("Sesión guardada en {}", path.display());
        Some(path)
    } else {
        session.discard()?;
        println!("No se detectó ninguna frase; no se ha guardado sesión.");
        None
    };
    let playback_result = playback.finish();
    result?;
    playback_result?;
    Ok(path)
}
