//! Ventana de subtítulos superpuesta: siempre encima, sin bordes, fondo semitransparente y transparente al ratón.
//! Debe ejecutarse en el hilo principal (exigencia de winit); el pipeline vive en otros hilos y le habla por canal.

use crate::config::{SubtitlePosition, SubtitlesConfig};
use eframe::egui::{self, Color32, FontId, Pos2, Vec2, ViewportCommand};
use std::{
    collections::VecDeque,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, RecvTimeoutError},
    },
    time::{Duration, Instant},
};
use winit::platform::x11::EventLoopBuilderExtX11;

pub enum SubtitleEvent {
    /// Frase traducida.
    Line {
        id: u32,
        original: String,
        translated: String,
    },
    /// Hay voz sonando ahora mismo (para un indicador discreto mientras llega la traducción).
    Speaking(bool),
    /// Mensaje breve de estado o error (p. ej. "Esperando audio de Firefox…").
    Status(String),
    /// Cerrar la ventana y volver.
    Quit,
}

/// Bloquea hasta que llega `Quit`, se cierra el canal o el usuario cierra la ventana.
pub fn run_window(cfg: &SubtitlesConfig, rx: Receiver<SubtitleEvent>) -> anyhow::Result<()> {
    anyhow::ensure!(
        cfg.font_size.is_finite() && cfg.font_size > 0.0,
        "Tamaño de subtítulos no válido"
    );
    anyhow::ensure!(
        cfg.hide_after_secs.is_finite()
            && cfg.hide_after_secs > 0.0
            && cfg.hide_after_secs <= 86_400.0,
        "Duración de subtítulos no válida"
    );
    anyhow::ensure!(
        (0.0..=1.0).contains(&cfg.background_opacity),
        "Opacidad no válida"
    );
    anyhow::ensure!(
        cfg.width_fraction.is_finite() && cfg.width_fraction > 0.0 && cfg.width_fraction <= 1.0,
        "Ancho de subtítulos no válido"
    );
    anyhow::ensure!(cfg.max_lines > 0, "Se necesita al menos una frase visible");

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Voz — subtítulos")
            .with_inner_size([1.0, 1.0])
            .with_decorations(false)
            .with_resizable(false)
            .with_transparent(true)
            .with_active(false)
            .with_taskbar(false)
            .with_window_type(egui::X11WindowType::Utility)
            .with_always_on_top()
            .with_mouse_passthrough(true),
        renderer: eframe::Renderer::Glow,
        event_loop_builder: Some(Box::new(|builder| {
            builder.with_x11();
        })),
        persist_window: false,
        ..Default::default()
    };
    let state = Mutex::new(SubtitleState::default());
    let stopped = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let state = &state;
        let stopped = &stopped;
        let result = eframe::run_native(
            "voz-traductor-subtitulos",
            options,
            Box::new(|cc| {
                let ctx = cc.egui_ctx.clone();
                // El receptor bloquea fuera del hilo gráfico. El timeout solo permite unir
                // este hilo si se cierra la ventana mientras el productor sigue conectado.
                scope.spawn(move || {
                    while !stopped.load(Ordering::Relaxed) {
                        let event = match rx.recv_timeout(Duration::from_millis(200)) {
                            Ok(event) => event,
                            Err(RecvTimeoutError::Timeout) => continue,
                            Err(RecvTimeoutError::Disconnected) => SubtitleEvent::Quit,
                        };
                        let quit = matches!(event, SubtitleEvent::Quit);
                        state.lock().unwrap().apply(event, cfg, Instant::now());
                        ctx.request_repaint();
                        if quit {
                            break;
                        }
                    }
                });
                Ok(Box::new(SubtitleWindow {
                    cfg,
                    state,
                    geometry: None,
                    mapped: false,
                }))
            }),
        );
        stopped.store(true, Ordering::Relaxed);
        result
            .map_err(|error| anyhow::anyhow!("No se puede abrir la ventana de subtítulos: {error}"))
    })
}

struct SubtitleLine {
    original: String,
    translated: String,
    expires: Instant,
}

/// Si `Speaking(true)` lleva este tiempo sin que llegue ninguna frase, el punto
/// se oculta (música, ruido) hasta la siguiente frase o `Speaking(false)`.
const SPEAKING_STALE: Duration = Duration::from_secs(10);

#[derive(Default)]
struct SubtitleState {
    lines: VecDeque<SubtitleLine>,
    speaking: bool,
    /// Momento de referencia de la gracia (≈ última frase).
    speaking_since: Option<Instant>,
    status: Option<(String, Instant)>,
    quit: bool,
}

impl SubtitleState {
    fn apply(&mut self, event: SubtitleEvent, cfg: &SubtitlesConfig, now: Instant) {
        match event {
            SubtitleEvent::Line {
                id: _,
                original,
                translated,
            } => {
                self.lines.push_back(SubtitleLine {
                    original,
                    translated,
                    expires: now + Duration::from_secs_f32(cfg.hide_after_secs),
                });
                while self.lines.len() > cfg.max_lines {
                    self.lines.pop_front();
                }
                if self.speaking {
                    self.speaking_since = Some(now);
                }
            }
            SubtitleEvent::Speaking(value) => {
                self.speaking = value;
                self.speaking_since = value.then_some(now);
            }
            SubtitleEvent::Status(text) => self.status = Some((text, now + Duration::from_secs(4))),
            SubtitleEvent::Quit => self.quit = true,
        }
    }

    /// Hay frases que se ven ahora mismo.
    fn has_visible_lines(&self, now: Instant) -> bool {
        self.lines.iter().any(|line| line.expires > now)
    }

    /// Punto de "hay voz": solo con subtítulos ya visibles y dentro del tiempo
    /// de gracia desde la última frase. Sin frases visibles no se dibuja nada
    /// (ventana totalmente transparente).
    fn speaking_indicator(&self, now: Instant) -> bool {
        self.speaking
            && self
                .speaking_since
                .is_some_and(|since| now.saturating_duration_since(since) <= SPEAKING_STALE)
            && self.has_visible_lines(now)
    }

    fn expire(&mut self, now: Instant) -> Option<Duration> {
        self.lines.retain(|line| line.expires > now);
        if self
            .status
            .as_ref()
            .is_some_and(|(_, expires)| *expires <= now)
        {
            self.status = None;
        }
        self.lines
            .iter()
            .map(|line| line.expires)
            .chain(self.status.as_ref().map(|(_, expires)| *expires))
            .min()
            .map(|expires| expires.saturating_duration_since(now))
    }
}

struct SubtitleWindow<'a> {
    cfg: &'a SubtitlesConfig,
    state: &'a Mutex<SubtitleState>,
    geometry: Option<(Pos2, Vec2)>,
    mapped: bool,
}

impl eframe::App for SubtitleWindow<'_> {
    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        // eframe crea la ventana oculta: winit no aplica el nivel X11 al crearla.
        // Reaplicarlo después del primer mapeo evita que quede detrás de GNOME.
        if !self.mapped {
            if let Some(window) = frame.winit_window()
                && window.is_visible() == Some(true)
            {
                window.set_window_level(winit::window::WindowLevel::AlwaysOnTop);
                window.request_user_attention(None);
                self.mapped = true;
            } else {
                ctx.request_repaint_after(Duration::from_millis(50));
            }
        }
        let mut state = self.state.lock().unwrap();
        if state.quit {
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }
        if let Some(delay) = state.expire(Instant::now()) {
            ctx.request_repaint_after(delay);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let Some(monitor) = frame
            .winit_window()
            .and_then(|window| window.current_monitor())
        else {
            return;
        };
        let scale = f64::from(ui.ctx().pixels_per_point());
        let screen = monitor.size().to_logical::<f32>(scale);
        let origin = monitor.position().to_logical::<f32>(scale);
        let margin = 24.0_f32.min(screen.height / 10.0);
        let width = (screen.width * self.cfg.width_fraction)
            .min(screen.width - margin * 2.0)
            .max(1.0);
        let padding = 16.0_f32.min(width / 4.0);
        let text_width = (width - padding * 2.0).max(1.0);
        let painter = ui.painter();
        let now = Instant::now();
        let state = self.state.lock().unwrap();
        let mut rows = Vec::new();
        for line in &state.lines {
            if self.cfg.show_original {
                rows.push(painter.layout(
                    line.original.clone(),
                    FontId::proportional(self.cfg.font_size * 0.7),
                    Color32::from_gray(190),
                    text_width,
                ));
            }
            rows.push(painter.layout(
                line.translated.clone(),
                FontId::proportional(self.cfg.font_size),
                Color32::WHITE,
                text_width,
            ));
        }
        // Sin frases visibles no se dibuja nada (nada de caja con "…" que tape el vídeo);
        // el estado sí puede salir solo, pequeño y con su caducidad.
        let speaking_dot = state.speaking_indicator(now);
        if let Some((text, _)) = &state.status {
            rows.push(painter.layout(
                text.clone(),
                FontId::proportional(self.cfg.font_size * 0.6),
                Color32::from_gray(190),
                text_width,
            ));
        }
        let height = if rows.is_empty() {
            1.0
        } else {
            (padding * 2.0 + rows.iter().map(|row| row.size().y + 6.0).sum::<f32>() - 6.0)
                .min(screen.height - margin * 2.0)
        };
        let size = egui::vec2(width, height);
        let y = match self.cfg.position {
            SubtitlePosition::Top => margin,
            SubtitlePosition::Bottom => screen.height - height - margin,
        };
        let position = egui::pos2(origin.x + (screen.width - width) / 2.0, origin.y + y);
        if self.geometry != Some((position, size)) {
            ui.ctx().send_viewport_cmd(ViewportCommand::InnerSize(size));
            ui.ctx()
                .send_viewport_cmd(ViewportCommand::OuterPosition(position));
            self.geometry = Some((position, size));
        }
        if rows.is_empty() {
            return;
        }
        painter.rect_filled(
            egui::Rect::from_min_size(Pos2::ZERO, size),
            12.0,
            Color32::from_black_alpha((self.cfg.background_opacity * 255.0).round() as u8),
        );
        let mut y = padding;
        for row in rows {
            let pos = egui::pos2((width - row.size().x) / 2.0, y);
            painter.galley_with_override_text_color(
                pos + egui::vec2(1.5, 1.5),
                row.clone(),
                Color32::BLACK,
            );
            y += row.size().y + 6.0;
            painter.galley(pos, row, Color32::WHITE);
        }
        if speaking_dot {
            painter.circle_filled(
                egui::pos2(width - 9.0, 9.0),
                3.0,
                Color32::from_rgb(120, 210, 160),
            );
        }
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0; 4]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limita_frases_y_caduca_cada_una_por_separado() {
        let cfg = SubtitlesConfig {
            max_lines: 2,
            hide_after_secs: 3.0,
            ..Default::default()
        };
        let mut state = SubtitleState::default();
        let now = Instant::now();
        for id in 0..3 {
            state.apply(
                SubtitleEvent::Line {
                    id,
                    original: id.to_string(),
                    translated: id.to_string(),
                },
                &cfg,
                now + Duration::from_secs(u64::from(id)),
            );
        }
        assert_eq!(state.lines.len(), 2);
        assert_eq!(state.lines[0].translated, "1");
        assert_eq!(
            state.expire(now + Duration::from_secs(4)),
            Some(Duration::from_secs(1))
        );
        assert_eq!(state.lines[0].translated, "2");
        assert_eq!(state.expire(now + Duration::from_secs(5)), None);
        assert!(state.lines.is_empty());
    }

    #[test]
    fn estado_dura_cuatro_segundos_y_quit_cierra() {
        let cfg = SubtitlesConfig::default();
        let now = Instant::now();
        let mut state = SubtitleState::default();
        state.apply(SubtitleEvent::Status("Esperando audio…".into()), &cfg, now);
        state.apply(SubtitleEvent::Speaking(true), &cfg, now);
        assert_eq!(state.expire(now), Some(Duration::from_secs(4)));
        assert_eq!(state.expire(now + Duration::from_secs(4)), None);
        assert!(state.status.is_none());
        assert!(state.speaking);
        state.apply(SubtitleEvent::Quit, &cfg, now);
        assert!(state.quit);
    }

    #[test]
    fn sin_frases_visibles_no_hay_indicador_de_voz() {
        let cfg = SubtitlesConfig::default();
        let now = Instant::now();
        let mut state = SubtitleState::default();
        state.apply(SubtitleEvent::Speaking(true), &cfg, now);
        assert!(!state.speaking_indicator(now));
        assert!(!state.has_visible_lines(now));
    }

    #[test]
    fn indicador_solo_dentro_de_la_caja_con_frases_visibles() {
        let cfg = SubtitlesConfig {
            hide_after_secs: 5.0,
            ..Default::default()
        };
        let now = Instant::now();
        let mut state = SubtitleState::default();
        state.apply(SubtitleEvent::Speaking(true), &cfg, now);
        state.apply(
            SubtitleEvent::Line {
                id: 1,
                original: "Hello".into(),
                translated: "Hola".into(),
            },
            &cfg,
            now,
        );
        assert!(state.speaking_indicator(now));
        // Al caducar la última frase, el punto desaparece con la caja.
        state.expire(now + Duration::from_secs(6));
        assert!(!state.speaking_indicator(now + Duration::from_secs(6)));
    }

    #[test]
    fn speaking_de_mas_de_diez_segundos_se_ignora() {
        let cfg = SubtitlesConfig {
            hide_after_secs: 60.0,
            ..Default::default()
        };
        let now = Instant::now();
        let mut state = SubtitleState::default();
        state.apply(SubtitleEvent::Speaking(true), &cfg, now);
        state.apply(
            SubtitleEvent::Line {
                id: 1,
                original: "Hi".into(),
                translated: "Hola".into(),
            },
            &cfg,
            now,
        );
        assert!(state.speaking_indicator(now + Duration::from_secs(10)));
        assert!(!state.speaking_indicator(now + Duration::from_secs(11)));
        // Un Speaking(false) y otro true reanudan el indicador.
        state.apply(
            SubtitleEvent::Speaking(false),
            &cfg,
            now + Duration::from_secs(11),
        );
        state.apply(
            SubtitleEvent::Speaking(true),
            &cfg,
            now + Duration::from_secs(11),
        );
        state.apply(
            SubtitleEvent::Line {
                id: 2,
                original: "Bye".into(),
                translated: "Adiós".into(),
            },
            &cfg,
            now + Duration::from_secs(12),
        );
        assert!(state.speaking_indicator(now + Duration::from_secs(12)));
    }

    #[test]
    fn una_frase_nueva_reanuda_el_indicador_tras_la_gracia() {
        let cfg = SubtitlesConfig {
            hide_after_secs: 60.0,
            ..Default::default()
        };
        let now = Instant::now();
        let mut state = SubtitleState::default();
        state.apply(SubtitleEvent::Speaking(true), &cfg, now);
        state.apply(
            SubtitleEvent::Line {
                id: 1,
                original: "One".into(),
                translated: "Uno".into(),
            },
            &cfg,
            now,
        );
        assert!(!state.speaking_indicator(now + Duration::from_secs(11)));
        state.apply(
            SubtitleEvent::Line {
                id: 2,
                original: "Two".into(),
                translated: "Dos".into(),
            },
            &cfg,
            now + Duration::from_secs(12),
        );
        assert!(state.speaking_indicator(now + Duration::from_secs(12)));
    }

    #[test]
    fn status_sale_solo_y_caduca_a_los_cuatro_segundos() {
        let cfg = SubtitlesConfig::default();
        let now = Instant::now();
        let mut state = SubtitleState::default();
        state.apply(SubtitleEvent::Status("Esperando audio…".into()), &cfg, now);
        assert!(state.status.is_some());
        assert!(!state.speaking_indicator(now));
        assert_eq!(state.expire(now + Duration::from_secs(5)), None);
        assert!(state.status.is_none());
    }
}
