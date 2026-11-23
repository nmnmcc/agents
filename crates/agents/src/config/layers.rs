use super::{
    inside, read_text,
    values::{References, is_reference},
};
use anyhow::{Context, Result, ensure};
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};
use std::{fs, io, path::Path};

const FORMATS: [&str; 5] = ["json", "json5", "jsonc", "yaml", "yml"];
const JSONC: jsonc_parser::ParseOptions = jsonc_parser::ParseOptions {
    allow_comments: true,
    allow_trailing_commas: true,
    allow_loose_object_property_names: false,
    allow_missing_commas: false,
    allow_single_quoted_strings: false,
    allow_hexadecimal_numbers: false,
    allow_unary_plus_numbers: false,
    allow_bare_decimal_point_numbers: false,
    allow_non_finite_numbers: false,
    allow_extended_string_escapes: false,
};

pub fn load<T: DeserializeOwned>(root: &Path) -> Result<Option<(T, References)>> {
    let mut files = Vec::new();
    for format in FORMATS {
        let name = format!(".agents.{format}");
        let name = Path::new(&name);
        if exists(&root.join(name))? {
            ensure!(
                files.is_empty(),
                "multiple .agents main files in {}",
                root.display()
            );
            files.push((inside(root, name)?, format));
        }
    }
    if exists(&root.join(".agents.d"))? {
        let dir = inside(root, Path::new(".agents.d"))?;
        let mut layers = dir
            .read_dir()
            .with_context(|| format!("read {}", dir.display()))?
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<io::Result<Vec<_>>>()?;
        layers.sort_unstable();
        for name in layers {
            let name = Path::new(&name);
            if let Some(format) = FORMATS
                .iter()
                .find(|format| name.extension().is_some_and(|ext| ext == **format))
            {
                files.push((inside(&dir, name)?, *format));
            }
        }
    }
    if files.is_empty() {
        return Ok(None);
    }

    let mut merged = Map::new();
    for (file, format) in &files {
        let text = read_text(file)?;
        let layer: Map<String, Value> = match *format {
            "json5" => parse_json5(&text)
                .with_context(|| format!("parse JSON5 object in {}", file.display()))?,
            "jsonc" => jsonc_parser::parse_to_serde_value(&text, &JSONC)
                .with_context(|| format!("parse JSONC object in {}", file.display()))?,
            "yaml" | "yml" => serde_yaml_ng::from_str(&text)
                .with_context(|| format!("parse YAML object in {}", file.display()))?,
            _ => serde_json::from_str(&text)
                .with_context(|| format!("parse JSON object in {}", file.display()))?,
        };
        merge(&mut merged, layer, &mut Vec::new());
    }
    apply_defaults(&mut merged)?;
    let mut references = References::default();
    let mut config = Value::Object(merged);
    references.expand(root, &mut config)?;
    let config = serde_json::from_value(config)
        .map_err(|error| {
            if references.used {
                anyhow::anyhow!("invalid configuration with external references")
            } else {
                error.into()
            }
        })
        .with_context(|| {
            format!(
                "decode project configuration from {}",
                files
                    .iter()
                    .map(|(path, _)| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
    Ok(Some((config, references)))
}

fn apply_defaults(config: &mut Map<String, Value>) -> Result<()> {
    for section in ["agents", "mcp", "harnesses"] {
        let Some(entries) = config.get_mut(section).and_then(Value::as_object_mut) else {
            continue;
        };
        let Some(defaults) = entries.remove("default") else {
            continue;
        };
        let defaults = defaults
            .as_object()
            .with_context(|| format!("{section}.default must be an object"))?;
        for (name, value) in entries {
            if let Some(settings) = value.as_object_mut() {
                let mut combined = defaults.clone();
                merge(
                    &mut combined,
                    std::mem::take(settings),
                    &mut vec![section.into(), name.clone()],
                );
                *settings = combined;
            }
        }
    }
    Ok(())
}

fn parse_json5(text: &str) -> Result<Map<String, Value>> {
    // Preserve non-finite numbers until validation: serde_json::Value silently
    // converts NaN and Infinity to null, which would change layer semantics.
    let object: std::collections::BTreeMap<String, serde_yaml_ng::Value> = json5::from_str(text)?;
    fn check(value: &serde_yaml_ng::Value) -> Result<()> {
        use serde_yaml_ng::Value as Parsed;
        match value {
            Parsed::Number(number) => ensure!(
                number.is_finite(),
                "non-finite numbers are not supported in configuration"
            ),
            Parsed::Sequence(values) => values.iter().try_for_each(check)?,
            Parsed::Mapping(values) => values.values().try_for_each(check)?,
            _ => {}
        }
        Ok(())
    }
    object.values().try_for_each(check)?;
    object
        .into_iter()
        .map(|(key, value)| Ok((key, serde_json::to_value(value)?)))
        .collect()
}

fn exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("inspect {}", path.display())),
    }
}

fn merge(base: &mut Map<String, Value>, layer: Map<String, Value>, path: &mut Vec<String>) {
    for (key, value) in layer {
        path.push(key.clone());
        match (base.get_mut(&key), value) {
            (Some(Value::Object(previous)), Value::Object(next))
                if !is_reference(previous, path) && !is_reference(&next, path) =>
            {
                merge(previous, next, path);
            }
            (_, next) => {
                base.insert(key, next);
            }
        }
        path.pop();
    }
}
