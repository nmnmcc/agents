use crate::{
    config::values::{ValueSource, read_env},
    types::EnvName,
};
use anyhow::{Context, Result, ensure};
use lettre::{
    SmtpTransport, Transport, address::Envelope, transport::smtp::authentication::Credentials,
};
use schemars::JsonSchema;
use serde::Deserialize;
use std::{
    io::{Read, Write},
    net::{IpAddr, TcpStream},
    path::Path,
};

#[derive(Default, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum Security {
    #[default]
    Tls,
    /// Test/local delivery only; requires a literal loopback address.
    Loopback,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    host: String,
    port: u16,
    #[serde(default)]
    security: Security,
}

impl Endpoint {
    fn check(&self) -> Result<()> {
        if matches!(self.security, Security::Loopback) {
            ensure!(
                self.host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback()),
                "plaintext mail transport requires a literal loopback address"
            );
        }
        Ok(())
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Login {
    Source(SourceLogin),
    Legacy(LegacyLogin),
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SourceLogin {
    username: String,
    password: ValueSource,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LegacyLogin {
    username: String,
    password_env: EnvName,
}

impl Login {
    fn username(&self) -> &str {
        match self {
            Self::Source(login) => &login.username,
            Self::Legacy(login) => &login.username,
        }
    }

    pub fn password_env(&self) -> Option<&EnvName> {
        match self {
            Self::Source(login) => login.password.env_name(),
            Self::Legacy(login) => Some(&login.password_env),
        }
    }

    fn password(&self, root: &Path) -> Result<String> {
        match self {
            Self::Source(login) => login.password.read(root),
            Self::Legacy(login) => read_env(&login.password_env),
        }
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Incoming {
    pub endpoint: Endpoint,
    pub login: Login,
    pub mailbox: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Outgoing {
    pub endpoint: Endpoint,
    pub login: Option<Login>,
}

trait Stream: Read + Write + Send {}
impl<T: Read + Write + Send> Stream for T {}

pub fn connect(config: &Incoming, root: &Path) -> Result<Session> {
    let endpoint = &config.endpoint;
    endpoint.check()?;
    let password = config.login.password(root)?;
    let stream =
        TcpStream::connect((endpoint.host.as_str(), endpoint.port)).context("connect to IMAP")?;
    let stream: Box<dyn Stream> = match endpoint.security {
        Security::Loopback => Box::new(stream),
        Security::Tls => Box::new(
            native_tls::TlsConnector::new()?
                .connect(&endpoint.host, stream)
                .context("establish IMAP TLS")?,
        ),
    };
    let mut client = imap::Client::new(stream);
    client.read_greeting().context("read IMAP greeting")?;
    let session = client
        .login(config.login.username(), password)
        .map_err(|(error, _)| error)
        .context("IMAP login")?;
    Ok(Session { session })
}

pub enum Submission {
    Accepted,
    Temporary,
    Rejected,
}

pub struct Session {
    session: imap::Session<Box<dyn Stream>>,
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.session.logout();
    }
}

impl Session {
    pub fn select(&mut self, name: &str, keywords: &[&str]) -> Result<u32> {
        let mailbox = self
            .session
            .select(name)
            .with_context(|| format!("select IMAP mailbox {name}"))?;
        ensure!(
            mailbox
                .permanent_flags
                .contains(&imap::types::Flag::MayCreate)
                || keywords
                    .iter()
                    .all(|name| has_flag(&mailbox.permanent_flags, name)),
            "IMAP mailbox {name} must support permanent Agents... keywords"
        );
        mailbox
            .uid_validity
            .context("IMAP did not report UIDVALIDITY")
    }

    pub fn search(&mut self, query: &str) -> Result<Vec<u32>> {
        let mut uids: Vec<_> = self
            .session
            .uid_search(query)
            .context("IMAP UID SEARCH")?
            .into_iter()
            .collect();
        uids.sort_unstable();
        Ok(uids)
    }

    pub fn message(&mut self, uid: u32) -> Result<Option<Vec<u8>>> {
        let data = self
            .session
            .uid_fetch(uid.to_string(), "(UID BODY.PEEK[])")
            .with_context(|| format!("fetch IMAP message UID {uid}"))?;
        Ok(data
            .iter()
            .find(|message| message.uid == Some(uid))
            .and_then(|message| message.body())
            .map(<[u8]>::to_vec))
    }

    pub fn flags(&mut self, uid: u32) -> Result<Vec<String>> {
        let data = self
            .session
            .uid_fetch(uid.to_string(), "(UID FLAGS)")
            .with_context(|| format!("fetch IMAP flags for UID {uid}"))?;
        let entry = data
            .iter()
            .find(|message| message.uid == Some(uid))
            .context("IMAP message disappeared")?;
        Ok(entry.flags().iter().map(ToString::to_string).collect())
    }

    pub fn set(&mut self, uid: u32, flag: &str) -> Result<()> {
        self.session
            .uid_store(uid.to_string(), format!("+FLAGS.SILENT ({flag})"))
            .with_context(|| format!("set IMAP flag {flag} on UID {uid}"))?;
        ensure!(
            self.flags(uid)?
                .iter()
                .any(|value| value.eq_ignore_ascii_case(flag)),
            "IMAP did not persist {flag} for UID {uid}"
        );
        Ok(())
    }

    pub fn set_read(&mut self, uid: u32, read: bool) -> Result<()> {
        let operation = if read {
            "+FLAGS.SILENT (\\Seen)"
        } else {
            "-FLAGS.SILENT (\\Seen)"
        };
        self.session.uid_store(uid.to_string(), operation)?;
        Ok(())
    }

    pub fn append(
        &mut self,
        mailbox: &str,
        flag: Option<&'static str>,
        message: &[u8],
    ) -> Result<()> {
        let mut append = self.session.append(mailbox, message);
        if let Some(flag) = flag {
            append.flag(imap::types::Flag::Custom(flag.into()));
        }
        append
            .finish()
            .with_context(|| format!("append to IMAP mailbox {mailbox}"))?;
        Ok(())
    }
}

fn has_flag(flags: &[imap::types::Flag<'_>], name: &str) -> bool {
    flags.iter().any(
        |flag| matches!(flag, imap::types::Flag::Custom(value) if value.eq_ignore_ascii_case(name)),
    )
}

pub fn submission(config: &Outgoing, root: &Path) -> Result<SmtpTransport> {
    let endpoint = &config.endpoint;
    endpoint.check()?;
    let mut builder = match endpoint.security {
        Security::Tls => SmtpTransport::relay(&endpoint.host)?,
        Security::Loopback => {
            ensure!(config.login.is_none(), "SMTP authentication requires TLS");
            SmtpTransport::builder_dangerous(&endpoint.host)
        }
    }
    .port(endpoint.port);
    if let Some(login) = &config.login {
        builder = builder.credentials(Credentials::new(
            login.username().into(),
            login.password(root)?,
        ));
    }
    Ok(builder.build())
}

pub fn send(smtp: &SmtpTransport, from: &str, to: &str, raw: &[u8]) -> Result<Submission> {
    let envelope = Envelope::new(Some(from.parse()?), vec![to.parse()?])?;
    Ok(match smtp.send_raw(&envelope, raw) {
        Ok(response) if u16::from(response.code()) == 250 => Submission::Accepted,
        Err(error) if error.is_permanent() => Submission::Rejected,
        Ok(_) | Err(_) => Submission::Temporary,
    })
}
