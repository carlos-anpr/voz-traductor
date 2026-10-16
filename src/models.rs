//! Descarga de modelos que no vienen con el binario.

use std::io::Write;
use std::path::{Path, PathBuf};

const KOKORO_BASE_URL: &str =
    "https://github.com/thewh1teagle/kokoro-onnx/releases/download/model-files-v1.0/";
const KOKORO_ONNX: &str = "kokoro-v1.0.fp16.onnx";
const KOKORO_VOICES: &str = "voices-v1.0.bin";

/// Garantiza `kokoro-v1.0.fp16.onnx` y `voices-v1.0.bin` en `dir`, descargándolos si faltan.
/// Devuelve (ruta del modelo, ruta de las voces).
pub fn ensure_kokoro_files(dir: &Path) -> anyhow::Result<(PathBuf, PathBuf)> {
    std::fs::create_dir_all(dir)
        .map_err(|e| anyhow::anyhow!("no se pudo crear {}: {}", dir.display(), e))?;
    let onnx = dir.join(KOKORO_ONNX);
    let voices = dir.join(KOKORO_VOICES);
    if !is_present(&onnx) {
        download(&format!("{KOKORO_BASE_URL}{KOKORO_ONNX}"), &onnx)?;
    }
    if !is_present(&voices) {
        download(&format!("{KOKORO_BASE_URL}{KOKORO_VOICES}"), &voices)?;
    }
    Ok((onnx, voices))
}

fn is_present(path: &Path) -> bool {
    path.is_file()
        && std::fs::metadata(path)
            .map(|m| m.len() > 0)
            .unwrap_or(false)
}

/// Descarga `url` a `dest` vía fichero `.part` + rename atómico.
/// Muestra el progreso en una sola línea de stderr y valida el tamaño final
/// contra `Content-Length` cuando el servidor lo envía.
fn download(url: &str, dest: &Path) -> anyhow::Result<()> {
    let mut resp = reqwest::blocking::get(url)
        .map_err(|e| anyhow::anyhow!("no se pudo descargar {url}: {e}"))?
        .error_for_status()
        .map_err(|e| anyhow::anyhow!("no se pudo descargar {url}: {e}"))?;
    let total = resp.content_length();
    let part = dest.with_extension("part");
    let mut out = std::fs::File::create(&part)
        .map_err(|e| anyhow::anyhow!("no se pudo crear {}: {}", part.display(), e))?;
    let mut done: u64 = 0;
    let mut last_pct: u64 = 101;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        use std::io::Read as _;
        let n = resp.read(&mut buf).map_err(|e| {
            std::fs::remove_file(&part).ok();
            anyhow::anyhow!("no se pudo descargar {url}: {e}")
        })?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])
            .map_err(|e| anyhow::anyhow!("no se pudo escribir {}: {}", part.display(), e))?;
        done += n as u64;
        if let Some(total) = total
            && total > 0
        {
            let pct = done.saturating_mul(100) / total;
            if pct != last_pct {
                last_pct = pct;
                eprint!("\rDescargando modelo de voz… {pct} %");
                let _ = std::io::stderr().flush();
            }
        }
    }
    out.flush()
        .map_err(|e| anyhow::anyhow!("no se pudo volcar {}: {}", part.display(), e))?;
    drop(out);
    if let Some(total) = total
        && done != total
    {
        std::fs::remove_file(&part).ok();
        return Err(anyhow::anyhow!(
            "descarga incompleta de {}: {done}/{total} bytes",
            dest.display()
        ));
    }
    eprintln!();
    std::fs::rename(&part, dest)
        .map_err(|e| anyhow::anyhow!("no se pudo guardar {}: {e}", dest.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn existentes_no_tocan_la_red() {
        let root = std::env::temp_dir().join(format!(
            "voz-traductor-kokoro-{}-{}",
            std::process::id(),
            chrono::Local::now().timestamp_millis()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(KOKORO_ONNX), b"modelo").unwrap();
        std::fs::write(root.join(KOKORO_VOICES), b"voces").unwrap();
        // Si intentase descargar, fallaría (sin red en el test); debe devolver las rutas sin más.
        let (onnx, voices) = ensure_kokoro_files(&root).unwrap();
        assert_eq!(onnx, root.join(KOKORO_ONNX));
        assert_eq!(voices, root.join(KOKORO_VOICES));
        std::fs::remove_dir_all(&root).ok();
    }
}
