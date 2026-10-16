//! Demostración sin captura ni modelos: frases cada dos segundos y cierre automático.
#[allow(dead_code)]
#[path = "../src/config.rs"]
mod config;
#[allow(dead_code)]
#[path = "../src/subtitles.rs"]
mod subtitles;

use std::{sync::mpsc, thread, time::Duration};
use subtitles::SubtitleEvent;

fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args().collect();
    let cfg = config::SubtitlesConfig {
        show_original: true,
        position: if args.iter().any(|arg| arg == "--arriba") {
            config::SubtitlePosition::Top
        } else {
            config::SubtitlePosition::Bottom
        },
        ..Default::default()
    };
    let (tx, rx) = mpsc::channel();
    let producer = thread::spawn(move || {
        let send = |event| tx.send(event).is_ok();
        if !send(SubtitleEvent::Status("Preparando subtítulos…".into())) {
            return;
        }
        thread::sleep(Duration::from_secs(2));
        for (id, (original, translated)) in [
            ("Welcome to the meeting.", "Bienvenidos a la reunión."),
            (
                "Today we will review the project and agree on the next steps together.",
                "Hoy revisaremos el proyecto y acordaremos juntos los próximos pasos.",
            ),
            (
                "Please share your questions before we finish. Thank you for joining us today.",
                "Compartid vuestras preguntas antes de terminar. Gracias por acompañarnos hoy.",
            ),
        ]
        .into_iter()
        .enumerate()
        {
            if !send(SubtitleEvent::Speaking(true)) {
                return;
            }
            thread::sleep(Duration::from_secs(2));
            if !send(SubtitleEvent::Line {
                id: id as u32,
                original: original.into(),
                translated: translated.into(),
            }) {
                return;
            }
            if !send(SubtitleEvent::Speaking(false)) {
                return;
            }
        }
        // Permite observar las caducidades y la desaparición completa de la caja.
        thread::sleep(Duration::from_secs(10));
        send(SubtitleEvent::Quit);
    });
    let result = subtitles::run_window(&cfg, rx);
    // No esperar al productor si la apertura de la ventana falla.
    if result.is_ok() {
        producer.join().expect("productor de demostración");
    }
    result
}
