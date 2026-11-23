use anyhow::{Context, Result, bail, ensure};
use clap::ValueEnum;
use std::{
    env, fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

#[derive(Clone, Copy, ValueEnum)]
pub enum Platform {
    Systemd,
    Launchd,
}

impl Platform {
    pub fn native() -> Result<Self> {
        match env::consts::OS {
            "linux" => Ok(Self::Systemd),
            "macos" => Ok(Self::Launchd),
            os => bail!("no user service manager supported on {os}"),
        }
    }
}

pub struct Definition {
    platform: Platform,
    name: String,
    pub text: String,
}

impl Definition {
    pub fn new(platform: Platform, root: &Path, executable: &Path) -> Result<Self> {
        let root = root.to_str().context("project path is not UTF-8")?;
        let executable = executable
            .to_str()
            .context("executable path is not UTF-8")?;
        // Stable FNV-1a identifier: no registry, and equal directory names do not collide by name.
        let id = root.bytes().fold(0xcbf29ce484222325u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
        });
        let (name, text) = match platform {
            Platform::Systemd => {
                ensure!(
                    !root.contains(['\n', '\r']) && !root.ends_with(['\\', ' ', '\t']),
                    "project path cannot be represented as systemd WorkingDirectory"
                );
                ensure!(
                    !executable.chars().any(char::is_control),
                    "executable path contains control characters"
                );
                let name = format!("agents-{id:016x}.service");
                let root = root.replace('%', "%%");
                let executable = executable
                    .replace('%', "%%")
                    .replace('\\', "\\\\")
                    .replace('"', "\\\"");
                let text = format!(
                    "# {name}\n\
                     [Unit]\nDescription=agents project mail service\n\n\
                     [Service]\nType=simple\nWorkingDirectory={root}\n\
                     ExecStart=:\"{executable}\"\nRestart=on-failure\nRestartSec=5\n\n\
                     [Install]\nWantedBy=default.target\n"
                );
                (name, text)
            }
            Platform::Launchd => {
                let name = format!("org.agents.project.{id:016x}");
                let root = xml(root)?;
                let executable = xml(executable)?;
                let text = format!(
                    r#"<?xml version="1.0" encoding="UTF-8"?>
<!-- {name}.plist -->
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{name}</string>
    <key>WorkingDirectory</key>
    <string>{root}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{executable}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
</dict>
</plist>
"#
                );
                (name, text)
            }
        };
        Ok(Self {
            platform,
            name,
            text,
        })
    }

    pub fn install(&self) -> Result<PathBuf> {
        match self.platform {
            Platform::Systemd => {
                let config = match env::var_os("XDG_CONFIG_HOME")
                    .map(PathBuf::from)
                    .filter(|path| path.is_absolute())
                {
                    Some(path) => path,
                    None => home()?.join(".config"),
                };
                let path = config.join("systemd/user").join(&self.name);
                let replaced = self.write(&path)?;
                checked(Command::new("systemctl").args(["--user", "daemon-reload"]))?;
                if replaced {
                    checked(Command::new("systemctl").args(["--user", "try-restart", &self.name]))?;
                }
                checked(Command::new("systemctl").args(["--user", "enable", "--now", &self.name]))?;
                Ok(path)
            }
            Platform::Launchd => {
                let output = Command::new("id").arg("-u").output().context("run id -u")?;
                ensure!(output.status.success(), "id -u failed: {}", output.status);
                let uid: u32 = std::str::from_utf8(&output.stdout)?
                    .trim()
                    .parse()
                    .context("read current user ID")?;
                let domain = format!("gui/{uid}");
                let target = format!("{domain}/{}", self.name);
                let path = home()?
                    .join("Library/LaunchAgents")
                    .join(format!("{}.plist", self.name));
                let replaced = self.write(&path)?;
                checked(Command::new("launchctl").args(["enable", &target]))?;
                let loaded = Command::new("launchctl")
                    .args(["print", &target])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .context("run launchctl print")?
                    .success();
                if loaded && replaced {
                    checked(Command::new("launchctl").args(["bootout", &target]))?;
                }
                if !loaded || replaced {
                    checked(
                        Command::new("launchctl")
                            .args(["bootstrap", &domain])
                            .arg(&path),
                    )?;
                }
                Ok(path)
            }
        }
    }

    // Returns whether an existing definition changed and needs a reload.
    fn write(&self, path: &Path) -> Result<bool> {
        let replaced = match fs::read(path) {
            Ok(contents) if contents == self.text.as_bytes() => return Ok(false),
            Ok(_) => true,
            Err(error) if error.kind() == io::ErrorKind::NotFound => false,
            Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
        };
        let parent = path.parent().context("service file has no parent")?;
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        file.write_all(self.text.as_bytes())?;
        file.persist(path)
            .with_context(|| format!("install {}", path.display()))?;
        Ok(replaced)
    }
}

fn home() -> Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .context("HOME must be an absolute path")
}

fn checked(command: &mut Command) -> Result<()> {
    let status = command
        .stdin(Stdio::null())
        .status()
        .with_context(|| format!("run {command:?}"))?;
    ensure!(status.success(), "{command:?} failed: {status}");
    Ok(())
}

fn xml(text: &str) -> Result<String> {
    let mut escaped = String::new();
    for ch in text.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '\r' => escaped.push_str("&#13;"),
            '\t' | '\n' | '\u{20}'..='\u{d7ff}' | '\u{e000}'..='\u{fffd}' | '\u{10000}'.. => {
                escaped.push(ch);
            }
            _ => bail!("path contains a character not supported in a launchd plist"),
        }
    }
    Ok(escaped)
}
