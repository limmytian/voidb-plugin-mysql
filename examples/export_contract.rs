//! Export and check the process manifest against the service capability contracts.
use std::{fs, path::Path};
use voidb_core::{ProcessPluginCapability, ProcessPluginManifest};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let check = std::env::args().any(|arg| arg == "--check");
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifest_path = root.join("plugin.toml");
    let mut manifest: ProcessPluginManifest = toml::from_str(&fs::read_to_string(&manifest_path)?)?;
    let mut contracts = Vec::new();
    for definition in voidb_plugin_mysql::mysql_capabilities() {
        let mut value = serde_json::to_value(&definition)?;
        for (kind, schema) in [
            ("input", &definition.input_schema),
            ("output", &definition.output_schema),
        ] {
            let relative = format!("schemas/{}-{kind}.schema.json", definition.id);
            let path = root.join(&relative);
            if check {
                let actual: serde_json::Value = serde_json::from_str(&fs::read_to_string(&path)?)?;
                if actual != *schema {
                    return Err(format!("schema drift: {relative}").into());
                }
            } else {
                fs::write(path, serde_json::to_string_pretty(schema)? + "\n")?;
            }
            value[format!("{kind}_schema")] = relative.into();
        }
        contracts.push(serde_json::from_value::<ProcessPluginCapability>(value)?);
    }
    if !check {
        manifest.capabilities = contracts.clone();
    }
    let exported: toml::Value = toml::from_str(&toml::to_string(&manifest)?)?;
    voidb_core::process_plugin::validate_process_plugin_manifest_json(&serde_json::to_value(
        exported,
    )?)?;
    if check {
        if manifest.capabilities != contracts {
            return Err("manifest capability contract drift".into());
        }
        println!("Manifest and service capability contracts match.");
    } else {
        manifest.capabilities = contracts;
        fs::write(manifest_path, toml::to_string_pretty(&manifest)?)?;
    }
    Ok(())
}
