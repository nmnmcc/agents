use super::{inside, read_text};
use crate::types::EnvName;
use anyhow::{Context, Result};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value};
use std::{
    collections::BTreeSet,
    env,
    path::{Path, PathBuf},
};

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EnvReference {
    env: EnvName,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FileReference {
    file: PathBuf,
}

#[derive(Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum ValueSource {
    Literal(String),
    Env(EnvReference),
    File(FileReference),
}

impl ValueSource {
    pub fn env_name(&self) -> Option<&EnvName> {
        match self {
            Self::Env(reference) => Some(&reference.env),
            _ => None,
        }
    }

    pub fn read(&self, root: &Path) -> Result<String> {
        match self {
            Self::Literal(value) => Ok(value.clone()),
            Self::Env(reference) => env::var(reference.env.to_string())
                .with_context(|| format!("read environment variable {}", reference.env)),
            Self::File(reference) => {
                let path = if reference.file.is_absolute() {
                    reference.file.clone()
                } else {
                    inside(root, &reference.file)?
                };
                read_text(&path)
            }
        }
    }
}

pub fn is_reference(object: &Map<String, Value>, path: &[String]) -> bool {
    let string_map = matches!(path, [registry, _, field]
        if (matches!(registry.as_str(), "mcp" | "harnesses") && field == "env")
            || (registry == "mcp" && field == "headers"));
    !string_map
        && object.len() == 1
        && ["env", "file"]
            .iter()
            .any(|key| object.get(*key).is_some_and(Value::is_string))
}

#[derive(Default)]
pub struct References {
    pub used: bool,
    pub env: BTreeSet<EnvName>,
    /// Referenced files, watched for configuration reloads.
    pub files: BTreeSet<PathBuf>,
}

impl References {
    pub fn expand(&mut self, root: &Path, value: &mut Value) -> Result<()> {
        self.walk(root, value, &mut Vec::new())
    }

    fn walk(&mut self, root: &Path, value: &mut Value, path: &mut Vec<String>) -> Result<()> {
        if matches!(path.as_slice(), [mail, transport, kind, login, password]
            if mail == "mail" && transport == "transport" && matches!(kind.as_str(), "imap" | "smtp")
                && login == "login" && password == "password")
        {
            // Reading credentials is only necessary when connecting to the mail server.
            return Ok(());
        }
        if matches!(path.as_slice(), [mcp, _] if mcp == "mcp")
            && value.get("disable") == Some(&Value::Bool(true))
        {
            // Disabled servers need not have working endpoints or secrets.
            return Ok(());
        }
        match value {
            Value::Object(object) if is_reference(object, path) => {
                let source: ValueSource = serde_json::from_value(value.take()).map_err(|_| {
                    anyhow::anyhow!("invalid env or file reference at {}", path.join("."))
                })?;
                if let Some(name) = source.env_name() {
                    self.env.insert(name.clone());
                }
                if let ValueSource::File(reference) = &source {
                    self.files.insert(root.join(&reference.file));
                }
                *value = Value::String(
                    source
                        .read(root)
                        .with_context(|| format!("resolve reference at {}", path.join(".")))?,
                );
                self.used = true;
            }
            Value::Object(object) => {
                for (key, child) in object {
                    path.push(key.clone());
                    self.walk(root, child, path)?;
                    path.pop();
                }
            }
            Value::Array(items) => {
                for (index, child) in items.iter_mut().enumerate() {
                    path.push(index.to_string());
                    self.walk(root, child, path)?;
                    path.pop();
                }
            }
            _ => {}
        }
        Ok(())
    }
}
