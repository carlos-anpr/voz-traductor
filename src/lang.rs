//! Detección del idioma de una frase transcrita, para no traducir lo que ya está en el idioma de destino
//! (eco de la propia traducción por los altavoces o compañeros hablando en inglés).

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use whatlang::{Detector, Lang};

/// Traducciones que han sonado hace poco. Si el micro vuelve a captar una de ellas (altavoces, prueba de
/// micro de Discord), la frase es eco y no se traduce: corta el bucle aunque sea muy corta.
#[derive(Default)]
pub struct EchoGuard {
    recent: Mutex<VecDeque<(Instant, Vec<String>)>>,
}

const ECHO_WINDOW: Duration = Duration::from_secs(30);
const ECHO_KEEP: usize = 8;

fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

impl EchoGuard {
    pub fn remember(&self, translation: &str) {
        let mut recent = self.recent.lock().unwrap_or_else(|e| e.into_inner());
        recent.push_back((Instant::now(), words(translation)));
        while recent.len() > ECHO_KEEP {
            recent.pop_front();
        }
    }

    /// Al menos la mitad de las palabras transcritas aparecen en una traducción reciente.
    pub fn is_echo(&self, transcript: &str) -> bool {
        let heard = words(transcript);
        if heard.is_empty() {
            return false;
        }
        let recent = self.recent.lock().unwrap_or_else(|e| e.into_inner());
        recent.iter().any(|(at, said)| {
            at.elapsed() < ECHO_WINDOW && {
                let hits = heard.iter().filter(|w| said.contains(w)).count();
                hits * 2 >= heard.len()
            }
        })
    }
}

pub struct TargetFilter {
    detector: Detector,
    source: Lang,
    target: Lang,
}

fn lang_by_name(name: &str) -> Option<Lang> {
    Lang::all()
        .iter()
        .copied()
        .find(|lang| lang.eng_name().eq_ignore_ascii_case(name.trim()))
}

impl TargetFilter {
    /// `None` si alguno de los idiomas no se reconoce (entonces no se filtra nada).
    pub fn new(source_lang: &str, target_lang: &str) -> Option<Self> {
        let source = lang_by_name(source_lang)?;
        let target = lang_by_name(target_lang)?;
        (source != target).then(|| Self {
            detector: Detector::with_allowlist(vec![source, target]),
            source,
            target,
        })
    }

    /// La frase está claramente en el idioma de origen (nunca se trata como eco).
    pub fn is_source(&self, text: &str) -> bool {
        self.detector
            .detect(text)
            .is_some_and(|info| info.lang() == self.source && info.confidence() >= 0.5)
    }

    /// La frase está en el idioma de destino y no hay que traducirla.
    pub fn is_target(&self, text: &str) -> bool {
        self.detector
            .detect(text)
            .is_some_and(|info| info.lang() == self.target && info.confidence() >= 0.2)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter() -> TargetFilter {
        TargetFilter::new("Spanish", "English").unwrap()
    }

    #[test]
    fn descarta_el_eco_en_ingles() {
        let f = filter();
        for text in [
            "We were expecting a pretty stately start up.",
            "Yesterday I was migrating the dates from Oracle to post SQL.",
            "Yesterday I was migrating the data from Oracle to post the SQL.",
            "Has anyone looked at the integration server logs this morning?",
        ] {
            assert!(f.is_target(text), "{text}");
        }
    }

    #[test]
    fn conserva_el_espanol_aunque_lleve_palabras_inglesas() {
        let f = filter();
        for text in [
            "Hola, ¿qué tal el día de hoy?",
            "No al día de hoy",
            "Se prevé un daily calentito.",
            "Ayer estuve emigrando la base de datos de Huracle a PostgreSQL.",
            "¿Alguien ha mirado los logs del servidor de integración esta mañana?",
            "Vale, pues lo dejamos así y lo comentamos en la daily.",
            "Mañana revisamos el despliegue.",
            "Sí.",
            "Vale.",
        ] {
            assert!(!f.is_target(text), "{text}");
        }
    }

    #[test]
    fn eco_de_una_traduccion_reciente() {
        let guard = EchoGuard::default();
        guard.remember("We're expecting a pretty intense daily stand-up.");
        guard.remember("Thank you.");
        assert!(guard.is_echo("We were expecting a pretty stately start up."));
        assert!(guard.is_echo("You were expecting a pretty intense time."));
        assert!(guard.is_echo("Thank you"));
        assert!(!guard.is_echo("Se prevé un daily calentito."));
        assert!(!guard.is_echo("Mañana revisamos el despliegue."));
        guard.remember("Yesterday we migrated Oracle to PostgreSQL.");
        assert!(!guard.is_echo("Hoy seguimos con Oracle y PostgreSQL."));
    }

    #[test]
    fn idiomas_desconocidos_o_iguales_no_filtran() {
        assert!(TargetFilter::new("Klingon", "English").is_none());
        assert!(TargetFilter::new("English", "English").is_none());
    }
}
