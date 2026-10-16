//! Traducción con LLMs vía API OpenAI-compatible (`POST {base_url}/chat/completions`),
//! con proveedores de reserva: si el principal falla, la misma frase pasa al siguiente.

use crate::config::{Config, TranslateConfig, TranslateProvider};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

static SESSION_ID: OnceLock<String> = OnceLock::new();

fn session_id() -> &'static str {
    SESSION_ID.get_or_init(|| {
        let pid = std::process::id();
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        format!("{millis:x}-{pid:x}")
    })
}

fn system_prompt(source: &str, target: &str) -> String {
    format!(
        "You are a translator for spoken meetings. Translate from {source} to {target} \
in natural, spoken {target}, as said in a meeting. \
Keep proper names and technical terms. \
Do not add explanations or comments: return only the translation. \
If the input is noise or makes no sense, return an empty string."
    )
}

/// Enfriamiento tras un 429 (o `Retry-After` si viene).
const COOLDOWN_LIMIT: Duration = Duration::from_secs(600);
/// Enfriamiento tras 401/403.
const COOLDOWN_AUTH: Duration = Duration::from_secs(1800);
/// Enfriamiento tras rachas de errores de red/timeout/5xx.
const COOLDOWN_FLAKY: Duration = Duration::from_secs(60);
const FLAKY_STREAK: u32 = 3;
/// Pausa del reintento heredado (un solo proveedor).
const LEGACY_RETRY_SLEEP: Duration = Duration::from_millis(300);

/// Resultado de traducir una frase: texto y quién la tradujo.
pub struct Translation {
    pub text: String,
    pub provider: String,
}

struct ProviderState {
    cfg: TranslateProvider,
    usable: bool,
    cooldown_until: Option<Instant>,
    /// Texto corto para el aviso (`durante 10 min`, …). Vacío si no hay enfriamiento.
    cooldown_desc: String,
    /// Fallos seguidos de red/timeout/5xx (a 3 → 60 s de enfriamiento).
    failures: u32,
}

pub struct Translator {
    client: reqwest::blocking::Client,
    prompt: String,
    context_phrases: usize,
    user_agent: String,
    providers: Vec<ProviderState>,
    /// Índice del último proveedor que tradujo bien (para avisar solo al cambiar).
    active: usize,
    /// Motivo del primer fallo de la frase en curso (para el aviso al cambiar).
    pending_note: Option<SwitchNote>,
}

struct SwitchNote {
    old: String,
    reason: String,
    cooldown_desc: String,
}

/// Error de un intento con un proveedor.
enum AttemptError {
    /// Red o timeout (`timeout` distingue el motivo).
    Transport { timeout: bool },
    /// HTTP no 2xx, con cuerpo y `Retry-After` (si venía).
    Status {
        code: u16,
        body: String,
        retry_after: Option<u64>,
    },
    /// 200 pero cuerpo inservible.
    BadResponse(String),
}

impl AttemptError {
    /// Motivo corto para el aviso de cambio (`límite agotado (429)`).
    fn short(&self) -> String {
        match self {
            Self::Transport { timeout: true } => "tiempo agotado".into(),
            Self::Transport { timeout: false } => "error de red".into(),
            Self::Status { code: 429, .. } => "límite agotado (429)".into(),
            Self::Status { code: 401, .. } => "no autorizado (401)".into(),
            Self::Status { code: 403, .. } => "prohibido (403)".into(),
            Self::Status { code: 400, .. } => {
                "petición rechazada (400; revisa la configuración)".into()
            }
            Self::Status { code, .. } => format!("HTTP {code}"),
            Self::BadResponse(_) => "respuesta inválida".into(),
        }
    }

    /// Si merece reintentar con el mismo proveedor (un solo proveedor: herencia).
    fn retryable(&self) -> bool {
        match self {
            Self::Transport { .. } => true,
            Self::Status { code, .. } => *code == 429 || (500..600).contains(code),
            Self::BadResponse(_) => false,
        }
    }

    /// Error completo para devolver al final (con cuerpo recortado, sin la clave).
    fn full(&self, provider: &str) -> anyhow::Error {
        let msg = match self {
            Self::Transport { timeout: true } => "tiempo de espera agotado".to_owned(),
            Self::Transport { timeout: false } => "error de conexión".to_owned(),
            Self::Status { code, body, .. } => {
                let short: String = body.chars().take(300).collect();
                format!("devolvió {code}: {}", short.trim())
            }
            Self::BadResponse(detail) => format!("respuesta inválida: {detail}"),
        };
        anyhow::anyhow!("el proveedor {provider} {msg}")
    }
}

impl Translator {
    pub fn new(cfg: &TranslateConfig) -> anyhow::Result<Self> {
        if cfg.providers.is_empty() {
            return Err(anyhow::anyhow!(
                "no hay proveedores de traducción configurados"
            ));
        }
        let client = reqwest::blocking::Client::builder()
            .build()
            .map_err(|e| anyhow::anyhow!("no se pudo crear el cliente HTTP: {e}"))?;
        let providers: Vec<ProviderState> = cfg
            .providers
            .iter()
            .map(|p| ProviderState {
                usable: p.api_key.as_deref().is_some_and(|k| !k.trim().is_empty()),
                cfg: p.clone(),
                cooldown_until: None,
                cooldown_desc: String::new(),
                failures: 0,
            })
            .collect();
        let mut usable = 0;
        for p in &providers {
            if !p.usable {
                eprintln!("proveedor {}: sin clave, se omite", p.cfg.name);
            } else {
                usable += 1;
            }
        }
        if usable == 0 {
            return Err(anyhow::anyhow!(
                "ningún proveedor de traducción tiene clave. Indícala por una de estas vías:\n\
                 1) `api_key` o `api_key_env` en cada `[[translate.providers]]` de {}\n\
                 2) con la variable de entorno VOZ_API_KEY\n\
                 3) en ~/.local/share/opencode/auth.json (campo [\"opencode-go\"][\"key\"]) si la URL es de opencode.ai",
                Config::path().display()
            ));
        }
        let active = providers.iter().position(|p| p.usable).unwrap_or(0);
        Ok(Self {
            client,
            prompt: system_prompt(&cfg.source_lang, &cfg.target_lang),
            context_phrases: cfg.context_phrases,
            user_agent: format!("voz-traductor/{}", env!("CARGO_PKG_VERSION")),
            providers,
            active,
            pending_note: None,
        })
    }

    /// Los subtítulos llegan por fragmentos; el contexto anterior no se repite.
    pub fn for_subtitles(cfg: &TranslateConfig) -> anyhow::Result<Self> {
        let mut translator = Self::new(cfg)?;
        translator.prompt.push_str("\nLive subtitles: input may start or end mid-sentence. Previous messages are context only; translate only the new fragment, without repeating earlier translations or completing missing text. Continue the previous clause when appropriate; preserve conjunctions and unfinished syntax instead of rewriting fragments as standalone sentences. ASR punctuation may be unreliable: do not invent sentence endings or add final punctuation when the thought continues.");
        Ok(translator)
    }

    /// Traduce `text` con el primer proveedor disponible; ante un error que lo
    /// justifique, la misma frase pasa al siguiente. `context` como hasta ahora.
    pub fn translate(
        &mut self,
        text: &str,
        context: &[(String, String)],
    ) -> anyhow::Result<Translation> {
        let active_name = self.providers[self.active].cfg.name.clone();
        if text.trim().is_empty() {
            return Ok(Translation {
                text: String::new(),
                provider: active_name,
            });
        }
        let now = Instant::now();
        let mut order: Vec<usize> = (0..self.providers.len())
            .filter(|&i| {
                self.providers[i].usable
                    && self.providers[i]
                        .cooldown_until
                        .is_none_or(|until| now >= until)
            })
            .collect();
        let cooling: Vec<usize> = {
            let mut v: Vec<usize> = (0..self.providers.len())
                .filter(|&i| {
                    self.providers[i].usable
                        && self.providers[i]
                            .cooldown_until
                            .is_some_and(|until| now < until)
                })
                .collect();
            v.sort_by_key(|&i| self.providers[i].cooldown_until);
            v
        };
        // Si todos están en enfriamiento, se prueba el que antes salga de él.
        order.extend(cooling);
        let single = order.len() == 1;
        self.pending_note = None;
        let mut last_err: Option<anyhow::Error> = None;
        for (n, &i) in order.iter().enumerate() {
            let name = self.providers[i].cfg.name.clone();
            match self.attempt(i, text, context) {
                Ok(clean) => {
                    self.on_success(i);
                    return Ok(Translation {
                        text: clean,
                        provider: name,
                    });
                }
                Err(e) => {
                    self.on_failure(i, &e);
                    last_err = Some(e.full(&name));
                    if n + 1 < order.len() {
                        continue;
                    }
                    if single && e.retryable() {
                        // Un solo proveedor: se conserva el reintento heredado.
                        std::thread::sleep(LEGACY_RETRY_SLEEP);
                        let name = self.providers[i].cfg.name.clone();
                        match self.attempt(i, text, context) {
                            Ok(clean) => {
                                self.on_success(i);
                                return Ok(Translation {
                                    text: clean,
                                    provider: name,
                                });
                            }
                            Err(e2) => {
                                self.on_failure(i, &e2);
                                return Err(e2.full(&name));
                            }
                        }
                    }
                    break;
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("error de traducción desconocido")))
    }

    fn attempt(
        &self,
        i: usize,
        text: &str,
        context: &[(String, String)],
    ) -> Result<String, AttemptError> {
        let p = &self.providers[i].cfg;
        let body = build_body(p, &self.prompt, self.context_phrases, text, context);
        let url = format!("{}/chat/completions", p.base_url.trim_end_matches('/'));
        let key = p.api_key.as_deref().unwrap_or("").trim().to_owned();
        let mut req = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {key}"))
            .header("User-Agent", &self.user_agent);
        if p.base_url.contains("opencode.ai") {
            req = req.header("x-opencode-session", session_id());
        }
        if p.timeout_secs > 0 {
            req = req.timeout(Duration::from_secs(p.timeout_secs));
        }
        let resp = req
            .json(&body)
            .send()
            .map_err(|e| AttemptError::Transport {
                timeout: e.is_timeout(),
            })?;
        let status = resp.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok())
                .filter(|&s| s > 0);
            let body = resp.text().unwrap_or_default();
            return Err(AttemptError::Status {
                code: 429,
                body,
                retry_after,
            });
        }
        if !status.is_success() {
            let code = status.as_u16();
            let body = resp.text().unwrap_or_default();
            return Err(AttemptError::Status {
                code,
                body,
                retry_after: None,
            });
        }
        let v: serde_json::Value = resp
            .json()
            .map_err(|e| AttemptError::BadResponse(format!("no es JSON: {e}")))?;
        let content = parse_content(&v).map_err(|e| AttemptError::BadResponse(e.to_string()))?;
        Ok(clean_translation(&content))
    }

    fn on_success(&mut self, i: usize) {
        self.providers[i].failures = 0;
        self.providers[i].cooldown_until = None;
        self.providers[i].cooldown_desc.clear();
        if i != self.active {
            let new = self.providers[i].cfg.name.clone();
            if let Some(note) = self.pending_note.take() {
                let during = if note.cooldown_desc.is_empty() {
                    String::new()
                } else {
                    format!(" durante {}", note.cooldown_desc)
                };
                eprintln!("{}: {}; usando {new}{during}", note.old, note.reason);
            } else if i == 0 {
                eprintln!("vuelta a {new} (principal disponible)");
            } else {
                eprintln!("usando {new}");
            }
            self.active = i;
        } else {
            self.pending_note = None;
        }
    }

    fn on_failure(&mut self, i: usize, e: &AttemptError) {
        let now = Instant::now();
        let mut cooled_desc = String::new();
        match e {
            AttemptError::Transport { .. } => {
                self.providers[i].failures += 1;
                if self.providers[i].failures >= FLAKY_STREAK {
                    self.providers[i].cooldown_until = Some(now + COOLDOWN_FLAKY);
                    cooled_desc = fmt_dur_es(COOLDOWN_FLAKY);
                }
            }
            AttemptError::Status {
                code: 429,
                retry_after,
                ..
            } => {
                self.providers[i].failures = 0;
                let wait = retry_after
                    .map(Duration::from_secs)
                    .unwrap_or(COOLDOWN_LIMIT);
                self.providers[i].cooldown_until = Some(now + wait);
                cooled_desc = fmt_dur_es(wait);
            }
            AttemptError::Status {
                code: 401 | 403, ..
            } => {
                self.providers[i].failures = 0;
                self.providers[i].cooldown_until = Some(now + COOLDOWN_AUTH);
                cooled_desc = fmt_dur_es(COOLDOWN_AUTH);
            }
            AttemptError::Status { code, .. } if (500..600).contains(code) => {
                self.providers[i].failures += 1;
                if self.providers[i].failures >= FLAKY_STREAK {
                    self.providers[i].cooldown_until = Some(now + COOLDOWN_FLAKY);
                    cooled_desc = fmt_dur_es(COOLDOWN_FLAKY);
                }
            }
            AttemptError::Status { .. } | AttemptError::BadResponse(_) => {}
        }
        self.providers[i].cooldown_desc = cooled_desc.clone();
        if self.pending_note.is_none() {
            self.pending_note = Some(SwitchNote {
                old: self.providers[self.active].cfg.name.clone(),
                reason: e.short(),
                cooldown_desc: cooled_desc,
            });
        }
    }
}

fn fmt_dur_es(d: Duration) -> String {
    let s = d.as_secs();
    if s.is_multiple_of(3600) {
        format!("{} h", s / 3600)
    } else if s.is_multiple_of(60) {
        format!("{} min", s / 60)
    } else {
        format!("{s} s")
    }
}

pub(crate) fn build_body(
    provider: &TranslateProvider,
    prompt: &str,
    context_phrases: usize,
    text: &str,
    context: &[(String, String)],
) -> serde_json::Value {
    let take = if context_phrases == 0 {
        0
    } else {
        context.len().min(context_phrases)
    };
    let recent = &context[context.len().saturating_sub(take)..];
    let mut messages = Vec::with_capacity(2 + recent.len() * 2);
    messages.push(serde_json::json!({
        "role": "system",
        "content": prompt,
    }));
    for (src, dst) in recent {
        messages.push(serde_json::json!({ "role": "user", "content": src }));
        messages.push(serde_json::json!({ "role": "assistant", "content": dst }));
    }
    messages.push(serde_json::json!({ "role": "user", "content": text }));
    let base = serde_json::json!({
        "model": provider.model,
        "temperature": 0.2,
        "max_tokens": 400,
        "messages": messages,
    });
    merge_body(base, &provider.extra_body)
}

pub(crate) fn merge_body(
    mut base: serde_json::Value,
    extra: &serde_json::Value,
) -> serde_json::Value {
    if let (Some(map), Some(obj)) = (base.as_object_mut(), extra.as_object()) {
        for (k, v) in obj {
            map.insert(k.clone(), v.clone());
        }
    }
    base
}

pub(crate) fn parse_content(v: &serde_json::Value) -> anyhow::Result<String> {
    let choice = v
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .ok_or_else(|| anyhow::anyhow!("respuesta sin choices de la API de traducción"))?;
    match choice.get("message").and_then(|m| m.get("content")) {
        None | Some(serde_json::Value::Null) => Ok(String::new()),
        Some(serde_json::Value::String(s)) => Ok(s.clone()),
        Some(other) => Ok(other.to_string()),
    }
}

pub(crate) fn clean_translation(s: &str) -> String {
    let mut t = s.trim().to_owned();
    for _ in 0..2 {
        let chars: Vec<char> = t.chars().collect();
        if chars.len() < 2 {
            break;
        }
        let first = chars[0];
        let last = chars[chars.len() - 1];
        let paired = (first == '"' && last == '"')
            || (first == '\'' && last == '\'')
            || (first == '“' && last == '”')
            || (first == '‘' && last == '’')
            || (first == '«' && last == '»')
            || (first == '`' && last == '`');
        if paired {
            t = chars[1..chars.len() - 1].iter().collect();
            t = t.trim().to_owned();
        } else {
            break;
        }
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fragment_prompt_is_exclusive_to_subtitles_and_keeps_context() {
        let cfg = test_cfg(vec![test_provider("a", "http://localhost:1", 5)]);
        let normal = Translator::new(&cfg).unwrap();
        let subtitles = Translator::for_subtitles(&cfg).unwrap();
        assert!(!normal.prompt.contains("fragment"));
        assert!(subtitles.prompt.contains("end mid-sentence"));
        assert!(
            subtitles
                .prompt
                .contains("without repeating earlier translations")
        );
        let body = build_body(
            &cfg.providers[0],
            &subtitles.prompt,
            1,
            "the connection is slow",
            &[("when".into(), "cuando".into())],
        );
        assert_eq!(body["messages"][1]["content"], "when");
        assert_eq!(body["messages"][2]["content"], "cuando");
        assert_eq!(body["messages"][3]["content"], "the connection is slow");
    }
    use std::io::{BufRead, Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn test_provider(name: &str, base_url: &str, timeout_secs: u64) -> TranslateProvider {
        TranslateProvider {
            name: name.into(),
            base_url: base_url.into(),
            model: format!("modelo-{name}"),
            api_key: Some("clave-prueba".into()),
            api_key_env: None,
            extra_body: serde_json::json!({}),
            timeout_secs,
        }
    }

    fn test_cfg(providers: Vec<TranslateProvider>) -> TranslateConfig {
        TranslateConfig {
            source_lang: "Spanish".into(),
            target_lang: "English".into(),
            context_phrases: 3,
            providers,
            base_url: String::new(),
            model: String::new(),
            api_key: None,
            extra_body: serde_json::Value::Null,
            timeout_secs: 0,
        }
    }

    fn ok_body(text: &str) -> String {
        format!("{{\"choices\":[{{\"message\":{{\"content\":\"{text}\"}}}}]}}")
    }

    /// Respuesta programada del servidor falso.
    #[derive(Clone)]
    struct Spec {
        status: u16,
        retry_after: Option<u64>,
        body: String,
        /// Retraso antes de responder (para probar timeouts).
        delay: Duration,
    }

    fn ok(text: &str) -> Spec {
        Spec {
            status: 200,
            retry_after: None,
            body: ok_body(text),
            delay: Duration::ZERO,
        }
    }

    fn err(status: u16) -> Spec {
        Spec {
            status,
            retry_after: None,
            body: format!("{{\"error\":\"falla {status}\"}}"),
            delay: Duration::ZERO,
        }
    }

    /// Servidor HTTP local en un hilo. Cada petición consume el siguiente `Spec`
    /// (el último se repite). Devuelve (base_url, contador de peticiones).
    fn spawn_fake(plan: Vec<Spec>) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let n = counter.fetch_add(1, Ordering::SeqCst);
                let spec = plan.get(n).unwrap_or_else(|| plan.last().unwrap()).clone();
                let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                let mut content_len = 0;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
                        break;
                    }
                    if let Some(v) = line.split_once(':')
                        && v.0.trim().eq_ignore_ascii_case("content-length")
                    {
                        content_len = v.1.trim().parse().unwrap_or(0);
                    }
                }
                let mut rest = vec![0u8; content_len];
                let _ = reader.read_exact(&mut rest);
                drop(reader);
                if !spec.delay.is_zero() {
                    std::thread::sleep(spec.delay);
                }
                let mut head = format!(
                    "HTTP/1.1 {} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
                    spec.status,
                    spec.body.len()
                );
                if let Some(ra) = spec.retry_after {
                    head += &format!("Retry-After: {ra}\r\n");
                }
                let _ = stream.write_all(format!("{head}\r\n{}", spec.body).as_bytes());
            }
        });
        (base_url, hits)
    }

    #[test]
    fn cuerpo_lleva_modelo_mensajes_y_contexto() {
        let ctx = vec![
            ("Hola".to_owned(), "Hello".to_owned()),
            ("¿Qué tal?".to_owned(), "How are you?".to_owned()),
        ];
        let p = test_provider("x", "http://localhost", 5);
        let body = build_body(
            &p,
            &system_prompt("Spanish", "English"),
            3,
            "Mañana seguimos",
            &ctx,
        );
        assert_eq!(body["model"], "modelo-x");
        assert_eq!(body["temperature"], 0.2);
        assert_eq!(body["max_tokens"], 400);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1 + 4 + 1);
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[1]["content"], "Hola");
        assert_eq!(msgs[2]["role"], "assistant");
        assert_eq!(msgs.last().unwrap()["content"], "Mañana seguimos");
    }

    #[test]
    fn fusion_respeta_claves_de_primer_nivel() {
        let base = serde_json::json!({"model": "a", "temperature": 0.2});
        let extra = serde_json::json!({"temperature": 0.9, "thinking": {"type": "disabled"}});
        let merged = merge_body(base, &extra);
        assert_eq!(merged["temperature"], 0.9);
        assert_eq!(merged["model"], "a");
        assert_eq!(merged["thinking"]["type"], "disabled");
    }

    #[test]
    fn parseo_lee_content_e_ignora_reasoning() {
        let v = serde_json::json!({
            "choices": [{
                "message": {"content": "Hello", "reasoning_content": "bla"},
                "finish_reason": "stop"
            }]
        });
        assert_eq!(parse_content(&v).unwrap(), "Hello");
    }

    #[test]
    fn parseo_sin_choices_da_error() {
        let v = serde_json::json!({"choices": []});
        assert!(parse_content(&v).is_err());
    }

    #[test]
    fn limpieza_quita_comillas_y_espacios() {
        assert_eq!(clean_translation("  \"Hello there\"  "), "Hello there");
        assert_eq!(clean_translation("'Hi'"), "Hi");
        assert_eq!(clean_translation("“Good morning”"), "Good morning");
        assert_eq!(clean_translation("  sin comillas  "), "sin comillas");
    }

    #[test]
    fn cambio_ante_429_y_enfriamiento() {
        let (a_url, a_hits) = spawn_fake(vec![Spec {
            retry_after: Some(60),
            ..err(429)
        }]);
        let (b_url, _) = spawn_fake(vec![ok("Hello")]);
        let mut t = Translator::new(&test_cfg(vec![
            test_provider("a", &a_url, 5),
            test_provider("b", &b_url, 5),
        ]))
        .unwrap();
        let r1 = t.translate("Hola", &[]).unwrap();
        assert_eq!((r1.text.as_str(), r1.provider.as_str()), ("Hello", "b"));
        assert_eq!(a_hits.load(Ordering::SeqCst), 1);
        // Enfriado 60 s: la segunda frase va directa al segundo sin tocar el primero.
        let r2 = t.translate("Adiós", &[]).unwrap();
        assert_eq!(r2.provider.as_str(), "b");
        assert_eq!(a_hits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn vuelta_al_principal_tras_enfriamiento() {
        let (a_url, a_hits) = spawn_fake(vec![
            Spec {
                retry_after: Some(1),
                ..err(429)
            },
            ok("Hello"),
        ]);
        let (b_url, _) = spawn_fake(vec![ok("Hi")]);
        let mut t = Translator::new(&test_cfg(vec![
            test_provider("a", &a_url, 5),
            test_provider("b", &b_url, 5),
        ]))
        .unwrap();
        assert_eq!(t.translate("Hola", &[]).unwrap().provider.as_str(), "b");
        assert_eq!(a_hits.load(Ordering::SeqCst), 1);
        std::thread::sleep(Duration::from_millis(1200));
        let r = t.translate("Hola", &[]).unwrap();
        assert_eq!((r.text.as_str(), r.provider.as_str()), ("Hello", "a"));
    }

    #[test]
    fn tres_500_seguidos_enfrian() {
        let (a_url, a_hits) = spawn_fake(vec![err(500)]);
        let (b_url, _) = spawn_fake(vec![ok("Hello")]);
        let mut t = Translator::new(&test_cfg(vec![
            test_provider("a", &a_url, 5),
            test_provider("b", &b_url, 5),
        ]))
        .unwrap();
        for _ in 0..2 {
            assert_eq!(t.translate("Hola", &[]).unwrap().provider.as_str(), "b");
        }
        assert_eq!(a_hits.load(Ordering::SeqCst), 2);
        // Tercer 500 → 60 s de enfriamiento: la siguiente ya no lo intenta.
        assert_eq!(t.translate("Hola", &[]).unwrap().provider.as_str(), "b");
        assert_eq!(a_hits.load(Ordering::SeqCst), 3);
        assert_eq!(t.translate("Hola", &[]).unwrap().provider.as_str(), "b");
        assert_eq!(a_hits.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn timeout_pasa_al_siguiente() {
        let (a_url, _) = spawn_fake(vec![Spec {
            delay: Duration::from_secs(5),
            ..ok("tarde")
        }]);
        let (b_url, _) = spawn_fake(vec![ok("Hello")]);
        let mut t = Translator::new(&test_cfg(vec![
            test_provider("a", &a_url, 1),
            test_provider("b", &b_url, 5),
        ]))
        .unwrap();
        let r = t.translate("Hola", &[]).unwrap();
        assert_eq!((r.text.as_str(), r.provider.as_str()), ("Hello", "b"));
    }

    #[test]
    fn error_401_enfria_sin_reintentar() {
        let (a_url, a_hits) = spawn_fake(vec![err(401)]);
        let (b_url, _) = spawn_fake(vec![ok("Hello")]);
        let mut t = Translator::new(&test_cfg(vec![
            test_provider("a", &a_url, 5),
            test_provider("b", &b_url, 5),
        ]))
        .unwrap();
        assert_eq!(t.translate("Hola", &[]).unwrap().provider.as_str(), "b");
        assert_eq!(a_hits.load(Ordering::SeqCst), 1);
        assert_eq!(t.translate("Hola", &[]).unwrap().provider.as_str(), "b");
        assert_eq!(a_hits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn error_400_pasa_al_siguiente() {
        let (a_url, _) = spawn_fake(vec![err(400)]);
        let (b_url, _) = spawn_fake(vec![ok("Hello")]);
        let mut t = Translator::new(&test_cfg(vec![
            test_provider("a", &a_url, 5),
            test_provider("b", &b_url, 5),
        ]))
        .unwrap();
        let r = t.translate("Hola", &[]).unwrap();
        assert_eq!((r.text.as_str(), r.provider.as_str()), ("Hello", "b"));
    }

    #[test]
    fn un_solo_proveedor_conserva_el_reintento() {
        let (a_url, a_hits) = spawn_fake(vec![err(500), ok("Hello")]);
        let mut t = Translator::new(&test_cfg(vec![test_provider("a", &a_url, 5)])).unwrap();
        let r = t.translate("Hola", &[]).unwrap();
        assert_eq!(r.text.as_str(), "Hello");
        assert_eq!(a_hits.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn proveedor_sin_clave_se_omite() {
        let (b_url, _) = spawn_fake(vec![ok("Hello")]);
        let mut no_key = test_provider("a", "http://localhost:1", 5);
        no_key.api_key = None;
        let mut t =
            Translator::new(&test_cfg(vec![no_key, test_provider("b", &b_url, 5)])).unwrap();
        let r = t.translate("Hola", &[]).unwrap();
        assert_eq!(r.provider.as_str(), "b");
    }

    #[test]
    fn sin_claves_no_arranca() {
        let mut no_key = test_provider("a", "http://localhost:1", 5);
        no_key.api_key = None;
        assert!(Translator::new(&test_cfg(vec![no_key])).is_err());
    }
}
