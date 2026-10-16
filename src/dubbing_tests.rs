use super::*;
#[test]
fn dubbing_requires_app_and_valid_finite_settings_before_startup() {
    let mut cfg = Config::default();
    cfg.oir.dub = true;
    assert!(cfg.oir.validate_dubbing().is_err());
    cfg.oir.app = Some(" ".into());
    assert!(cfg.oir.validate_dubbing().is_err());
    cfg.oir.app = Some("prueba".into());
    assert!(cfg.oir.validate_dubbing().is_ok());
    cfg.oir.duck_volume = f32::NAN;
    assert!(cfg.oir.validate_dubbing().is_err());
    cfg.oir.duck_volume = 0.25;
    cfg.oir.max_speed = 0.9;
    assert!(cfg.oir.validate_dubbing().is_err());
}
#[test]
fn queue_discards_oldest_and_speed_tracks_backlog() {
    let mut state = State::new();
    let now = Instant::now();
    assert_eq!(state.speed(now, 1.35), 1.0);
    state.pending.push_back(Job {
        id: 1,
        text: String::new(),
        estimate: 3.0,
    });
    state.ready.push_back(Audio {
        id: 2,
        samples: vec![0.0; TTS_SAMPLE_RATE as usize * 4],
        speed: 1.0,
    });
    assert_eq!(state.speed(now, 1.35), 1.35);
    state.limit(now);
    assert!(state.pending.is_empty());
    assert_eq!(state.stats.discarded, 1);
    assert_eq!(state.ready[0].id, 2);
    state.inflight = Some(Inflight {
        id: 3,
        estimate: 4.0,
        discarded: false,
    });
    state.limit(now);
    assert!(state.ready.is_empty());
    assert!(!state.inflight.as_ref().unwrap().discarded);
    assert!(state.queued(now) <= 6.0);
    assert!(state.speed(now, 1.35) > 1.0);
}
#[test]
fn active_audio_is_never_discarded_and_inflight_is_bounded() {
    let mut state = State::new();
    let now = Instant::now();
    state.playing_until = Some(now + Duration::from_secs(5));
    state.inflight = Some(Inflight {
        id: 1,
        estimate: 3.0,
        discarded: false,
    });
    state.limit(now);
    assert!(state.inflight.as_ref().unwrap().discarded);
    assert!(state.playing_until.is_some());
    assert_eq!(state.stats.discarded, 1);
}
#[test]
fn spanish_voice_selection_is_independent_of_microphone() {
    let mut cfg = Config::default();
    assert_eq!(voice_config(&cfg).unwrap().voice, "em_alex");
    cfg.tts.engine = TtsEngine::Edge;
    assert_eq!(voice_config(&cfg).unwrap().voice, "es-ES-AlvaroNeural");
    cfg.oir.dub_voice = Some("ef_dora".into());
    assert!(voice_config(&cfg).is_err());
    cfg.tts.engine = TtsEngine::Kokoro;
    assert_eq!(voice_config(&cfg).unwrap().voice, "ef_dora");
}
