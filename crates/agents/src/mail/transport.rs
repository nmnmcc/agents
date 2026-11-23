use crate::{
    config::values::ValueSource,
    types::{EnvName, MailAddress},
};
use anyhow::{Context, Result, ensure};
use lettre::{
    SmtpTransport, Transport, address::Envelope, transport::smtp::authentication::Credentials,
};
use schemars::JsonSchema;
use serde::Deserialize;
use std::{
    io::{self, Read, Write},
    net::{IpAddr, TcpStream},
    path::Path,
    time::Duration,
};

#[derive(Default, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum Security {
    #[default]
    Tls,
    /// Upgrade a plaintext SMTP connection with mandatory STARTTLS.
    Starttls,
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
#[serde(deny_unknown_fields)]
pub struct Login {
    username: String,
    password: ValueSource,
}

impl Login {
    pub fn password_env(&self) -> Option<&EnvName> {
        self.password.env_name()
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Incoming {
    pub endpoint: Endpoint,
    pub login: Login,
    pub mailbox: String,
    /// Seconds between checks for new mail when the server lacks IDLE, and
    /// the longest single IDLE wait otherwise.
    #[serde(default = "default_poll_interval")]
    #[schemars(range(min = 1))]
    pub poll_interval: u64,
}

fn default_poll_interval() -> u64 {
    60
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Outgoing {
    pub endpoint: Endpoint,
    pub login: Option<Login>,
    /// Shared SMTP From and envelope sender. Defaults to the sending agent's
    /// address. When they differ, the agent's address is used as Reply-To.
    pub from: Option<MailAddress>,
}

pub enum Connection {
    Plain(TcpStream),
    Tls(Box<native_tls::TlsStream<TcpStream>>),
}

impl Read for Connection {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Plain(stream) => stream.read(buffer),
            Self::Tls(stream) => stream.read(buffer),
        }
    }
}

impl Write for Connection {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        match self {
            Self::Plain(stream) => stream.write(buffer),
            Self::Tls(stream) => stream.write(buffer),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Plain(stream) => stream.flush(),
            Self::Tls(stream) => stream.flush(),
        }
    }
}

impl imap::extensions::idle::SetReadTimeout for Connection {
    fn set_read_timeout(&mut self, timeout: Option<Duration>) -> imap::Result<()> {
        match self {
            Self::Plain(stream) => stream.set_read_timeout(timeout)?,
            Self::Tls(stream) => stream.get_ref().set_read_timeout(timeout)?,
        }
        Ok(())
    }
}

pub fn connect(config: &Incoming, root: &Path) -> Result<Imap> {
    let endpoint = &config.endpoint;
    endpoint.check()?;
    ensure!(
        !matches!(endpoint.security, Security::Starttls),
        "IMAP requires implicit TLS, not STARTTLS"
    );
    let password = config.login.password.read(root)?;
    let stream =
        TcpStream::connect((endpoint.host.as_str(), endpoint.port)).context("connect to IMAP")?;
    let stream = match endpoint.security {
        Security::Loopback => Connection::Plain(stream),
        Security::Tls => Connection::Tls(Box::new(
            native_tls::TlsConnector::new()?
                .connect(&endpoint.host, stream)
                .context("establish IMAP TLS")?,
        )),
        Security::Starttls => anyhow::bail!("IMAP requires implicit TLS, not STARTTLS"),
    };
    let mut client = imap::Client::new(stream);
    client.read_greeting().context("read IMAP greeting")?;
    let session = client
        .login(&config.login.username, password)
        .map_err(|(error, _)| error)
        .context("IMAP login")?;
    Ok(Imap { session })
}

pub enum Submission {
    Accepted,
    Temporary,
    Rejected,
}

pub struct Imap {
    session: imap::Session<Connection>,
}

impl Drop for Imap {
    fn drop(&mut self) {
        let _ = self.session.logout();
    }
}

impl Imap {
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
        self.fetch_message(uid, "(UID BODY.PEEK[])")
    }

    pub fn message_prefix(&mut self, uid: u32, bytes: usize) -> Result<Option<Vec<u8>>> {
        self.fetch_message(uid, &format!("(UID BODY.PEEK[]<0.{bytes}>)"))
    }

    fn fetch_message(&mut self, uid: u32, query: &str) -> Result<Option<Vec<u8>>> {
        let data = self
            .session
            .uid_fetch(uid.to_string(), query)
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

    pub fn keyword(&mut self, uid: u32, flag: &str, add: bool) -> Result<()> {
        let operation = if add { '+' } else { '-' };
        self.session
            .uid_store(uid.to_string(), format!("{operation}FLAGS.SILENT ({flag})"))
            .with_context(|| format!("update IMAP flag {flag} on UID {uid}"))?;
        ensure!(
            self.flags(uid)?
                .iter()
                .any(|value| value.eq_ignore_ascii_case(flag))
                == add,
            "IMAP did not persist {flag} for UID {uid}"
        );
        Ok(())
    }

    pub fn expunge(&mut self, uids: &[u32]) -> Result<()> {
        let set = uids
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",");
        self.session
            .uid_store(&set, "+FLAGS.SILENT (\\Deleted)")
            .context("mark IMAP messages deleted")?;
        self.session
            .uid_expunge(&set)
            .context("expunge IMAP messages")?;
        Ok(())
    }

    pub fn supports_idle(&mut self) -> Result<bool> {
        Ok(self
            .session
            .capabilities()
            .context("IMAP CAPABILITY")?
            .has_str("IDLE"))
    }

    /// Waits until the selected mailbox changes or the timeout passes.
    pub fn idle(&mut self, timeout: Duration) -> Result<()> {
        self.session
            .idle()
            .timeout(timeout)
            .keepalive(false)
            .wait_while(|_| false)
            .context("IMAP IDLE")?;
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
        Security::Starttls => SmtpTransport::starttls_relay(&endpoint.host)?,
        Security::Loopback => {
            ensure!(config.login.is_none(), "SMTP authentication requires TLS");
            SmtpTransport::builder_dangerous(&endpoint.host)
        }
    }
    .port(endpoint.port);
    if let Some(login) = &config.login {
        builder = builder.credentials(Credentials::new(
            login.username.clone(),
            login.password.read(root)?,
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
