//! Micrófono virtual persistente sobre pipewire-pulse.
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::{path::PathBuf, process::Command, thread, time::Duration};

pub const SINK: &str = "voz-traductor";
pub const SOURCE: &str = "voz-traductor-mic";
pub const DESCRIPTION: &str = "Voz traducida (voz-traductor)";
const MARKER: &str = "# Micrófono virtual de voz-traductor.\n";
const SINK_ARGS: &str = "sink_name=voz-traductor rate=24000 channels=1 channel_map=mono format=float32le sink_properties='device.description=\"Voz traducida (voz-traductor)\" priority.session=0'";
const SOURCE_ARGS: &str = "master=voz-traductor.monitor source_name=voz-traductor-mic rate=24000 channels=1 channel_map=mono master_channel_map=mono remix=no source_properties='device.description=\"Voz traducida (voz-traductor)\" priority.session=0'";

fn pactl(args: &[&str]) -> Result<String> {
    let output = Command::new("pactl")
        .args(args)
        .output()
        .context("No se puede ejecutar pactl; instala pulseaudio-utils y activa pipewire-pulse")?;
    ensure!(
        output.status.success(),
        "Falló pactl {}: {}",
        args.first().copied().unwrap_or(""),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    String::from_utf8(output.stdout).context("pactl devolvió texto no válido")
}
fn list(kind: &str) -> Result<Vec<Value>> {
    serde_json::from_str(&pactl(&["-f", "json", "list", kind])?)
        .context("No se puede interpretar la respuesta de pactl")
}
fn device(kind: &str, name: &str) -> Result<Option<Value>> {
    Ok(list(kind)?
        .into_iter()
        .find(|v| v["name"].as_str() == Some(name)))
}
pub fn permanent_path() -> Result<PathBuf> {
    Ok(dirs::config_dir()
        .context("No se encuentra la carpeta de configuración")?
        .join("pipewire/pipewire-pulse.conf.d/90-voz-traductor.conf"))
}
fn permanent_config() -> String {
    let sink =
        serde_json::to_string(&format!("module-null-sink {SINK_ARGS}")).expect("cadena JSON");
    let source =
        serde_json::to_string(&format!("module-remap-source {SOURCE_ARGS}")).expect("cadena JSON");
    format!(
        "{MARKER}pulse.cmd = [\n    {{ cmd = \"load-module\" args = {sink} flags = [ nofail ] }}\n    {{ cmd = \"load-module\" args = {source} flags = [ nofail ] }}\n]\n"
    )
}
fn wait_for(kind: &str, name: &str) -> Result<Value> {
    for _ in 0..50 {
        if let Some(device) = device(kind, name)? {
            return Ok(device);
        }
        thread::sleep(Duration::from_millis(20));
    }
    bail!("PipeWire no ha creado {name}")
}
fn owned_module(device: &Value, module_name: &str, argument: &str) -> Result<String> {
    let id = device["owner_module"]
        .as_u64()
        .filter(|id| *id != u64::from(u32::MAX))
        .context("El dispositivo virtual no pertenece a un módulo Pulse")?
        .to_string();
    let modules = pactl(&["list", "short", "modules"])?;
    ensure!(
        modules.lines().any(|line| {
            let mut fields = line.split('\t');
            fields.next() == Some(id.as_str())
                && fields.next() == Some(module_name)
                && fields.next().is_some_and(|args| {
                    args.split_whitespace().any(|a| a == argument)
                        && (module_name != "module-remap-source"
                            || args
                                .split_whitespace()
                                .any(|a| a == "master=voz-traductor.monitor"))
                })
        }),
        "El nombre del dispositivo virtual está ocupado por otra configuración"
    );
    Ok(id)
}

/// Crea los módulos si faltan. No se descargan al salir del programa.
pub fn ensure() -> Result<bool> {
    let mut created = false;
    if let Some(sink) = device("sinks", SINK)? {
        owned_module(&sink, "module-null-sink", "sink_name=voz-traductor")?;
    } else {
        pactl(&["load-module", "module-null-sink", SINK_ARGS])?;
        wait_for("sinks", SINK)?;
        created = true;
    }
    if let Some(source) = device("sources", SOURCE)? {
        owned_module(
            &source,
            "module-remap-source",
            "source_name=voz-traductor-mic",
        )?;
    } else {
        pactl(&["load-module", "module-remap-source", SOURCE_ARGS])?;
        wait_for("sources", SOURCE)?;
        created = true;
    }
    let source = wait_for("sources", SOURCE)?;
    ensure!(
        source["description"].as_str() == Some(DESCRIPTION),
        "El micrófono virtual tiene una descripción inesperada"
    );
    Ok(created)
}

pub fn install() -> Result<()> {
    let path = permanent_path()?;
    if path.exists() {
        ensure!(
            std::fs::read_to_string(&path)?.starts_with(MARKER),
            "Ya existe {} con otra configuración; no se sobrescribe",
            path.display()
        );
    }
    std::fs::create_dir_all(path.parent().context("Ruta de configuración no válida")?)?;
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    std::fs::write(&temporary, permanent_config())?;
    std::fs::rename(&temporary, &path)?;
    ensure()?;
    println!(
        "Micrófono instalado: {DESCRIPTION}\nConfiguración permanente: {}",
        path.display()
    );
    Ok(())
}

pub fn remove() -> Result<()> {
    let path = permanent_path()?;
    if path.exists() {
        ensure!(
            std::fs::read_to_string(&path)?.starts_with(MARKER),
            "El fichero {} no pertenece a esta aplicación; no se elimina",
            path.display()
        );
    }
    // La fuente debe desaparecer antes de descargar el sink del que depende.
    for (kind, name, module, argument) in [
        (
            "sources",
            SOURCE,
            "module-remap-source",
            "source_name=voz-traductor-mic",
        ),
        ("sinks", SINK, "module-null-sink", "sink_name=voz-traductor"),
    ] {
        if let Some(device) = device(kind, name)? {
            let id = owned_module(&device, module, argument)?;
            pactl(&["unload-module", &id])?;
        }
    }
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    println!("Micrófono virtual quitado.");
    Ok(())
}

fn recording_apps(source: u64, outputs: &[Value]) -> Vec<String> {
    outputs
        .iter()
        .filter(|v| v["source"].as_u64() == Some(source))
        .map(|v| {
            let properties = &v["properties"];
            let name = properties["application.name"]
                .as_str()
                .or_else(|| properties["application.process.binary"].as_str())
                .unwrap_or("Aplicación sin nombre");
            let pid = properties["application.process.id"].as_str().unwrap_or("?");
            format!("{name} (PID {pid})")
        })
        .collect()
}
pub fn status() -> Result<()> {
    let source = device("sources", SOURCE)?;
    println!(
        "Micrófono virtual: {}",
        if source.is_some() {
            DESCRIPTION
        } else {
            "no existe"
        }
    );
    let path = permanent_path()?;
    println!("Permanente: {}", if path.is_file() { "sí" } else { "no" });
    if let Some(id) = source.as_ref().and_then(|v| v["index"].as_u64()) {
        let apps = recording_apps(id, &list("source-outputs")?);
        println!(
            "Grabando: {}",
            if apps.is_empty() {
                "ninguna aplicación".into()
            } else {
                apps.join(", ")
            }
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn permanent_config_keeps_module_order_and_quoted_description() {
        let config = permanent_config();
        assert!(config.starts_with(MARKER));
        assert!(
            config.find("module-null-sink").unwrap() < config.find("module-remap-source").unwrap()
        );
        for (line, args) in config
            .lines()
            .filter(|l| l.contains("args = "))
            .zip([SINK_ARGS, SOURCE_ARGS])
        {
            let json = line
                .split("args = ")
                .nth(1)
                .unwrap()
                .split(" flags = ")
                .next()
                .unwrap();
            let decoded: String = serde_json::from_str(json).unwrap();
            assert!(decoded.ends_with(args));
            assert!(decoded.contains(DESCRIPTION));
        }
    }
    #[test]
    fn status_reports_only_virtual_source_recorders() {
        let outputs = serde_json::json!([
            {"source":42,"properties":{"application.name":"Discord","application.process.id":"123"}},
            {"source":7,"properties":{"application.name":"Otro micrófono"}}
        ]);
        assert_eq!(
            recording_apps(42, outputs.as_array().unwrap()),
            ["Discord (PID 123)"]
        );
    }
}
