use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize, de};
use std::{
    borrow::Borrow,
    fmt,
    path::{Component, Path},
    str::FromStr,
};

#[derive(Clone, Copy)]
pub struct Version;

impl Version {
    const NUMBER: u32 = 1;
}

impl<'de> Deserialize<'de> for Version {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = u32::deserialize(deserializer)?;
        if value != Self::NUMBER {
            return Err(de::Error::custom(format!(
                "unsupported version {value}; expected {}",
                Self::NUMBER
            )));
        }
        Ok(Self)
    }
}

impl JsonSchema for Version {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Version".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type": "integer", "const": Self::NUMBER})
    }
}

macro_rules! parsed_string {
    ($name:ident, $valid:expr, $expected:literal, $schema:expr) => {
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl FromStr for $name {
            type Err = String;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                if ($valid)(value) {
                    Ok(Self(value.into()))
                } else {
                    Err(format!("expected {}, got {value:?}", $expected))
                }
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                String::deserialize(deserializer)?
                    .parse()
                    .map_err(de::Error::custom)
            }
        }

        impl JsonSchema for $name {
            fn schema_name() -> std::borrow::Cow<'static, str> {
                stringify!($name).into()
            }

            fn json_schema(_: &mut SchemaGenerator) -> Schema {
                $schema
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

parsed_string!(
    MailAddress,
    |s: &str| s.is_ascii()
        && !s
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
        && !s.contains(['"', '<', '>', '[', ']'])
        && email_address::EmailAddress::is_valid(s),
    "a bare ASCII mailbox address",
    json_schema!({"type": "string", "format": "email"})
);

impl MailAddress {
    pub fn matches(&self, other: &Self) -> bool {
        same_mailbox(&self.0, &other.0)
    }

    pub fn domain(&self) -> &str {
        self.0.split_once('@').expect("parsed mailbox").1
    }

    pub fn routing_key(&self) -> String {
        let (local, domain) = self.0.split_once('@').expect("parsed mailbox");
        format!("{local}@{}", domain.to_ascii_lowercase())
    }
}

fn same_mailbox(left: &str, right: &str) -> bool {
    let (local, domain) = left.split_once('@').expect("parsed mailbox");
    let (other_local, other_domain) = right.split_once('@').expect("parsed mailbox");
    local == other_local && domain.eq_ignore_ascii_case(other_domain)
}

parsed_string!(
    Id,
    id_segment,
    "an ID made of lowercase ASCII letters, digits, hyphens or underscores",
    json_schema!({"type": "string", "pattern": "^[a-z0-9_-]+$(?![\\s\\S])"})
);

fn id_segment(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_'))
}

impl Borrow<str> for Id {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl AsRef<Path> for Id {
    fn as_ref(&self) -> &Path {
        Path::new(&self.0)
    }
}

fn relative(s: &str) -> bool {
    !s.contains(['\\', '\0'])
        && s.split('/').all(|part| !matches!(part, "" | "." | ".."))
        && Path::new(s)
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
}

parsed_string!(
    AgentSelector,
    |s: &str| match s.rsplit_once(':') {
        Some((project, id)) => id_segment(id) && (project == "." || relative(project)),
        None => id_segment(s),
    },
    "an agent ID or a workspace-relative path:agent selector",
    json_schema!({
        "type": "string",
        "pattern": "^(?:[^\\\\\\u0000]+:)?[a-z0-9_-]+$(?![\\s\\S])",
        "description": "An agent ID in this project, or a workspace-relative path:agent selector."
    })
);

impl AgentSelector {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

parsed_string!(
    RelativePath,
    relative,
    "a relative file path without empty, . or .. components",
    json_schema!({
        "type": "string",
        "pattern": "^[^/\\\\\\u0000]+(?:/[^/\\\\\\u0000]+)*$(?![\\s\\S])",
        "not": {"pattern": "(^|/)\\.{1,2}(/|$)"},
        "description": "A slash-separated relative file path, resolved beneath its owning directory."
    })
);

impl AsRef<Path> for RelativePath {
    fn as_ref(&self) -> &Path {
        Path::new(&self.0)
    }
}

parsed_string!(
    FileName,
    |s: &str| relative(s) && !s.contains('/'),
    "one artifact filename",
    json_schema!({"type": "string", "pattern": "^(?:[^./\\\\\\u0000][^/\\\\\\u0000]*|\\.[^./\\\\\\u0000][^/\\\\\\u0000]*|\\.\\.[^/\\\\\\u0000]+)$(?![\\s\\S])"})
);

impl AsRef<Path> for FileName {
    fn as_ref(&self) -> &Path {
        Path::new(&self.0)
    }
}

parsed_string!(
    Program,
    |s: &str| !s.contains('\0')
        && (Path::new(s).is_absolute() || (relative(s) && !s.contains('/'))),
    "an absolute program path or bare executable name",
    json_schema!({
        "type": "string", "minLength": 1, "not": {"pattern": "\\u0000"},
        "description": "An absolute executable path or a bare executable name resolved via PATH."
    })
);

impl AsRef<std::ffi::OsStr> for Program {
    fn as_ref(&self) -> &std::ffi::OsStr {
        self.0.as_ref()
    }
}

parsed_string!(
    EnvName,
    |s: &str| {
        let mut bytes = s.bytes();
        bytes
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == b'_')
            && bytes.all(|c| c.is_ascii_alphanumeric() || c == b'_')
    },
    "an environment variable name",
    json_schema!({"type": "string", "pattern": "^[A-Za-z_][A-Za-z0-9_]*$(?![\\s\\S])"})
);

impl AsRef<std::ffi::OsStr> for EnvName {
    fn as_ref(&self) -> &std::ffi::OsStr {
        self.0.as_ref()
    }
}
