//! Genera un WAV inglés continuo con Kokoro, sin reproducirlo.
#[allow(dead_code)]
#[path = "../src/config.rs"]
mod config;
#[allow(dead_code)]
#[path = "../src/kokoro.rs"]
mod kokoro;
#[allow(dead_code)]
#[path = "../src/models.rs"]
mod models;
#[allow(dead_code)]
#[path = "../src/types.rs"]
mod types;

fn main() -> anyhow::Result<()> {
    let path = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("Indica el WAV de salida"))?;
    let text = include_str!("../tests/fixtures/narracion-en.txt");
    let mut engine = kokoro::Kokoro::load(&config::TtsConfig::default())?;
    // Sintetizar cláusulas cortas evita la degradación de Kokoro en entradas muy
    // largas; se concatenan con solo 80 ms, sin pausas que resuelvan el VAD viejo.
    let mut audio = Vec::new();
    for clause in text.split_inclusive([',', '.', ';']) {
        if clause.trim().is_empty() {
            continue;
        }
        audio.extend(engine.synthesize(clause.trim())?);
        audio.resize(
            audio.len() + (types::TTS_SAMPLE_RATE as usize * 80 / 1000),
            0.0,
        );
    }
    let mut writer = hound::WavWriter::create(
        path,
        hound::WavSpec {
            channels: 1,
            sample_rate: types::TTS_SAMPLE_RATE,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        },
    )?;
    for sample in &audio {
        writer.write_sample((sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16)?;
    }
    writer.finalize()?;
    println!(
        "{:.3} s, {} palabras",
        audio.len() as f64 / f64::from(types::TTS_SAMPLE_RATE),
        text.split_whitespace().count()
    );
    Ok(())
}
