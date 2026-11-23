use super::{Reply, compose, outbox::message_id};
use crate::types::MailAddress;
use anyhow::{Context, Result, ensure};
use mail_parser::{ContentType, MessageParser, MimeHeaders, PartType};
use schemars::JsonSchema;
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

pub(super) const LIMIT: usize = 8 * 1024 * 1024;

#[derive(Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct Signatures {
    /// Public keys exported with `gpg --export` (a binary keyring).
    openpgp_keyring: Option<PathBuf>,
    #[serde(default)]
    trusted: Vec<Trusted>,
    /// Private signing identity indexed by agent address, with the shared SMTP
    /// From address as a fallback.
    #[serde(default)]
    signing: BTreeMap<MailAddress, Signing>,
}

#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum Method {
    Openpgp,
    Smime,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Trusted {
    method: Method,
    address: MailAddress,
    /// OpenPGP primary-key fingerprint or SHA-256 fingerprint of an S/MIME certificate.
    fingerprint: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(tag = "method", rename_all = "snake_case", deny_unknown_fields)]
enum Signing {
    Openpgp {
        fingerprint: String,
        /// Private GnuPG home containing the signing key.
        home: PathBuf,
    },
    Smime {
        certificate: PathBuf,
        key: PathBuf,
    },
}

pub(super) struct Verified {
    pub sender: MailAddress,
    pub recipient: MailAddress,
    pub id: String,
    pub message: Vec<u8>,
}

impl Signatures {
    pub fn check(&self) -> Result<()> {
        let mut trusted = std::collections::BTreeSet::new();
        for entry in &self.trusted {
            let fingerprint = normalize(&entry.fingerprint);
            let length_ok = match entry.method {
                Method::Openpgp => {
                    ensure!(
                        self.openpgp_keyring.is_some(),
                        "trusted OpenPGP signers require a public keyring"
                    );
                    matches!(fingerprint.len(), 40 | 64)
                }
                Method::Smime => fingerprint.len() == 64,
            };
            ensure!(
                length_ok && fingerprint.bytes().all(|byte| byte.is_ascii_hexdigit()),
                "invalid trusted signing fingerprint"
            );
            ensure!(
                trusted.insert((entry.method, fingerprint)),
                "duplicate trusted signing fingerprint"
            );
        }
        let mut addresses = std::collections::BTreeSet::new();
        for (address, identity) in &self.signing {
            ensure!(
                addresses.insert(address.routing_key()),
                "duplicate SMTP signing address"
            );
            if let Signing::Openpgp { fingerprint, .. } = identity {
                ensure!(
                    matches!(fingerprint.len(), 40 | 64)
                        && fingerprint.bytes().all(|byte| byte.is_ascii_hexdigit()),
                    "invalid SMTP signing fingerprint"
                );
            }
        }
        Ok(())
    }

    pub fn verify(&self, root: &Path, raw: &[u8]) -> Result<Option<Verified>> {
        if raw.len() > LIMIT || self.trusted.is_empty() {
            return Ok(None);
        }
        let Some(message) = MessageParser::default().parse(raw) else {
            return Ok(None);
        };
        let Some(part) = message.parts.first() else {
            return Ok(None);
        };
        // Reject ambiguous MIME declarations instead of letting different verifiers pick one.
        if part
            .headers
            .iter()
            .filter(|header| header.name.as_str().eq_ignore_ascii_case("Content-Type"))
            .count()
            > 1
        {
            return Ok(None);
        }
        let mime = part.content_type();
        let is = |kind, subtype| mime.is_some_and(|m| content_type(m, kind, subtype));
        let protocol = mime.and_then(|m| m.attribute("protocol"));
        let uses = |expected: &str| protocol.is_some_and(|v| v.eq_ignore_ascii_case(expected));
        let signed = is("multipart", "signed");
        let recovered = if is("application", "pkcs7-mime")
            || is("application", "x-pkcs7-mime")
            || (signed
                && (uses("application/pkcs7-signature") || uses("application/x-pkcs7-signature")))
        {
            self.verify_smime(raw)?
        } else if signed && uses("application/pgp-signature") {
            let PartType::Multipart(children) = &part.body else {
                return Ok(None);
            };
            let [entity, signature] = children.as_slice() else {
                return Ok(None);
            };
            let (Some(entity), Some(signature)) = (
                message.parts.get(*entity as usize),
                message.parts.get(*signature as usize),
            ) else {
                return Ok(None);
            };
            if !signature
                .content_type()
                .is_some_and(|m| content_type(m, "application", "pgp-signature"))
            {
                return Ok(None);
            }
            let Some(entity) =
                raw.get(entity.raw_header_offset() as usize..entity.raw_end_offset() as usize)
            else {
                return Ok(None);
            };
            self.verify_pgp(root, &crlf(entity), Some(signature.contents()))?
        } else if let PartType::Text(text) = &part.body
            && text.starts_with("-----BEGIN PGP SIGNED MESSAGE-----")
        {
            if !clear_signed(text) {
                return Ok(None);
            }
            self.verify_pgp(root, text.as_bytes(), None)?
        } else {
            None
        };
        let Some((sender, body)) = recovered else {
            return Ok(None);
        };
        let Some(task) = Task::parse(&body) else {
            return Ok(None);
        };
        let recipient = task.recipient;
        let reply = task.parent.map(|parent| Reply {
            sender: sender.clone(),
            to: Vec::new(),
            cc: Vec::new(),
            subject: String::new(),
            parent: format!("<{parent}>"),
            references: task.references,
        });
        let message = compose(
            &sender,
            std::slice::from_ref(&recipient),
            &[],
            &recipient,
            &[],
            &task.subject,
            task.body,
            reply.as_ref(),
            &task.id,
        )?;
        Ok(Some(Verified {
            sender,
            recipient,
            id: task.id,
            message,
        }))
    }

    fn identity(&self, method: Method, fingerprint: &str) -> Option<MailAddress> {
        let fingerprint = normalize(fingerprint);
        self.trusted
            .iter()
            .find(|entry| entry.method == method && normalize(&entry.fingerprint) == fingerprint)
            .map(|entry| entry.address.clone())
    }

    fn verify_pgp(
        &self,
        root: &Path,
        data: &[u8],
        detached: Option<&[u8]>,
    ) -> Result<Option<(MailAddress, String)>> {
        let Some(keyring) = &self.openpgp_keyring else {
            return Ok(None);
        };
        let keyring =
            fs::canonicalize(root.join(keyring)).context("open OpenPGP public keyring")?;
        let dir = tempfile::tempdir()?;
        let input = dir.path().join("input");
        let signature = dir.path().join("signature");
        let plaintext = dir.path().join("verified");
        fs::write(&input, data)?;
        let mut command = gpg();
        command
            .arg("--homedir")
            .arg(dir.path())
            .args(["--lock-never", "--no-default-keyring", "--keyring"])
            .arg(keyring)
            .args(["--status-fd", "1"]);
        if let Some(bytes) = detached {
            fs::write(&signature, bytes)?;
            command.args(["--verify"]).arg(signature);
        } else {
            command
                .arg("--max-output")
                .arg(LIMIT.to_string())
                .args(["--decrypt", "--output"])
                .arg(&plaintext);
        }
        let result = run(command.arg(input))?;
        if !result.status.success() {
            return Ok(None);
        }
        let status = String::from_utf8_lossy(&result.stdout);
        let Some(fingerprint) = pgp_fingerprint(&status) else {
            return Ok(None);
        };
        let Some(sender) = self.identity(Method::Openpgp, fingerprint) else {
            return Ok(None);
        };
        let body = if detached.is_some() {
            mime_body(data)
        } else {
            String::from_utf8(fs::read(plaintext)?).ok().map(|text| {
                // OpenPGP cleartext signatures don't cover trailing spaces or tabs.
                text.lines()
                    .map(|line| line.trim_end_matches([' ', '\t']))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
        };
        Ok(body.map(|body| (sender, body)))
    }

    fn verify_smime(&self, raw: &[u8]) -> Result<Option<(MailAddress, String)>> {
        if !self
            .trusted
            .iter()
            .any(|entry| matches!(entry.method, Method::Smime))
        {
            return Ok(None);
        }
        let dir = tempfile::tempdir()?;
        let input = dir.path().join("input");
        let cert = dir.path().join("signer.pem");
        fs::write(&input, raw)?;
        // The leaf is pinned below; do not rely on the public CA store or the From header.
        // -noverify skips certificate chain validation, NOT the CMS content/signature checks.
        let verified = run(openssl()
            .args(["cms", "-verify", "-inform", "SMIME", "-noverify", "-in"])
            .arg(input)
            .arg("-signer")
            .arg(&cert))?;
        if !verified.status.success() {
            return Ok(None);
        }
        let cert_bytes = fs::read(&cert)?;
        if cert_bytes
            .split(|&byte| byte == b'\n')
            .filter(|line| {
                line.strip_suffix(b"\r").unwrap_or(line) == b"-----BEGIN CERTIFICATE-----"
            })
            .count()
            != 1
        {
            return Ok(None);
        }
        let details = run(openssl()
            .args(["x509", "-noout", "-sha256", "-fingerprint", "-in"])
            .arg(cert))?;
        if !details.status.success() {
            return Ok(None);
        }
        let details = String::from_utf8_lossy(&details.stdout);
        let Some((_, fingerprint)) = details.trim().split_once('=') else {
            return Ok(None);
        };
        let Some(sender) = self.identity(Method::Smime, fingerprint) else {
            return Ok(None);
        };
        Ok(mime_body(&verified.stdout).map(|body| (sender, body)))
    }

    pub fn sign(
        &self,
        root: &Path,
        agent: &MailAddress,
        from: &MailAddress,
        to: &MailAddress,
        id: &str,
        raw: &[u8],
    ) -> Result<Vec<u8>> {
        let identity = |address: &MailAddress| {
            self.signing
                .iter()
                .find(|(key, _)| key.matches(address))
                .map(|(_, signing)| signing)
        };
        let signing = identity(agent)
            .or_else(|| identity(from))
            .with_context(|| {
                format!("no SMTP signing identity configured for {agent} or {from}")
            })?;
        let message = MessageParser::default()
            .parse(raw)
            .context("invalid queued mail")?;
        let body = message.body_text(0).context("missing queued mail body")?;
        let mut protected = format!(
            "Agent-To: {to}\nTask-ID: {id}\nSubject: {}\n",
            message
                .subject()
                .unwrap_or("")
                .replace(['\r', '\n', '\0'], " "),
        );
        for name in ["In-Reply-To", "References"] {
            let value = match name {
                "In-Reply-To" => message.in_reply_to(),
                _ => message.references(),
            };
            if let Some(value) = value.as_text() {
                protected.push_str(&format!("{name}: <{value}>\n"));
            } else if let Some(values) = value.as_text_list() {
                protected.push_str(&format!(
                    "{name}: {}\n",
                    values
                        .iter()
                        .map(|value| format!("<{value}>"))
                        .collect::<Vec<_>>()
                        .join(" ")
                ));
            }
        }
        protected.push('\n');
        protected.push_str(&body);
        let dir = tempfile::tempdir()?;
        let input = dir.path().join("task");
        let mut signed = match signing {
            Signing::Openpgp { fingerprint, home } => {
                fs::write(&input, protected)?;
                let result = run(gpg()
                    .arg("--homedir")
                    .arg(root.join(home))
                    .args([
                        "--local-user",
                        fingerprint,
                        "--digest-algo",
                        "SHA256",
                        "--output",
                        "-",
                        "--clearsign",
                    ])
                    .arg(input))?;
                ensure!(result.status.success(), "OpenPGP signing failed");
                lettre::message::SinglePart::plain(String::from_utf8(result.stdout)?).formatted()
            }
            Signing::Smime { certificate, key } => {
                let part = lettre::message::SinglePart::plain(protected).formatted();
                fs::write(&input, part)?;
                let result = run(openssl()
                    .args([
                        "cms",
                        "-sign",
                        "-binary",
                        "-nodetach",
                        "-outform",
                        "SMIME",
                        "-in",
                    ])
                    .arg(input)
                    .arg("-signer")
                    .arg(root.join(certificate))
                    .arg("-inkey")
                    .arg(root.join(key)))?;
                ensure!(result.status.success(), "S/MIME signing failed");
                result.stdout
            }
        };
        let mut wire = format!("From: {from}\r\n").into_bytes();
        if !agent.matches(from) {
            wire.extend_from_slice(format!("Reply-To: {agent}\r\n").as_bytes());
        }
        for (name, value) in message.headers_raw() {
            if matches!(
                name.to_ascii_lowercase().as_str(),
                "to" | "cc" | "subject" | "date" | "message-id" | "in-reply-to" | "references"
            ) {
                wire.extend_from_slice(format!("{name}: {}", value.trim_start()).as_bytes());
                if !wire.ends_with(b"\n") {
                    wire.extend_from_slice(b"\r\n");
                }
            }
        }
        if matches!(signing, Signing::Openpgp { .. }) {
            wire.extend_from_slice(b"MIME-Version: 1.0\r\n");
        }
        wire.append(&mut signed);
        let wire = crlf(&wire);
        ensure!(wire.len() <= LIMIT, "signed mail exceeds size limit");
        Ok(wire)
    }
}

fn normalize(value: &str) -> String {
    value.replace(':', "").to_ascii_uppercase()
}

struct Task {
    recipient: MailAddress,
    id: String,
    subject: String,
    body: String,
    parent: Option<String>,
    references: String,
}

impl Task {
    fn parse(text: &str) -> Option<Self> {
        let text = text.replace("\r\n", "\n");
        let (headers, body) = text.split_once("\n\n")?;
        if headers.len() > 8192 || body.trim().is_empty() || text.contains(['\r', '\0']) {
            return None;
        }
        let mut fields = BTreeMap::new();
        for line in headers.lines() {
            let (key, value) = line.split_once(':')?;
            if !matches!(
                key,
                "Agent-To" | "Task-ID" | "Subject" | "In-Reply-To" | "References"
            ) || fields.insert(key, value.trim()).is_some()
            {
                return None;
            }
        }
        let id = message_id(fields.remove("Task-ID")?)?;
        let recipient = fields.remove("Agent-To")?.parse().ok()?;
        let subject = fields
            .remove("Subject")
            .unwrap_or("Signed agent task")
            .to_owned();
        let parent = match fields.remove("In-Reply-To") {
            Some(value) => Some(message_id(value)?),
            None => None,
        };
        let references = fields
            .remove("References")
            .unwrap_or("")
            .split_whitespace()
            .map(|value| message_id(value).map(|id| format!("<{id}>")))
            .collect::<Option<Vec<_>>>()?
            .join(" ");
        Some(Self {
            recipient,
            id,
            subject,
            body: body.to_owned(),
            parent,
            references,
        })
    }
}

fn pgp_fingerprint(status: &str) -> Option<&str> {
    let mut records = status
        .lines()
        .filter_map(|line| line.strip_prefix("[GNUPG:] "));
    let mut fingerprint = None;
    records.try_for_each(|record| {
        let kind = record.split_whitespace().next()?;
        match kind {
            "VALIDSIG" => {
                let fields: Vec<_> = record.split_whitespace().collect();
                if fingerprint.is_some()
                    || !matches!(*fields.get(8)?, "8" | "9" | "10" | "11" | "12" | "14")
                {
                    return None;
                }
                fingerprint = Some(*fields.get(10).or_else(|| fields.get(1))?);
            }
            "BADSIG" | "ERRSIG" | "EXPSIG" | "EXPKEYSIG" | "REVKEYSIG" | "NO_PUBKEY"
            | "KEYEXPIRED" | "SIGEXPIRED" | "FAILURE" | "DECRYPTION_OKAY" => return None,
            _ => {}
        }
        Some(())
    })?;
    fingerprint
}

fn clear_signed(text: &str) -> bool {
    let mut lines = text.lines();
    if lines.next() != Some("-----BEGIN PGP SIGNED MESSAGE-----") {
        return false;
    }
    let mut markers = lines
        .filter(|line| line.starts_with("-----BEGIN PGP ") || line.starts_with("-----END PGP "));
    markers.next() == Some("-----BEGIN PGP SIGNATURE-----")
        && markers.next() == Some("-----END PGP SIGNATURE-----")
        && markers.next().is_none()
        && text.trim_end().ends_with("\n-----END PGP SIGNATURE-----")
}

fn mime_body(entity: &[u8]) -> Option<String> {
    let parsed = MessageParser::default().parse(entity)?;
    let part = parsed.text_part(0)?;
    match &part.body {
        PartType::Text(text) => Some(text.to_string()),
        _ => None,
    }
}

fn content_type(mime: &ContentType<'_>, kind: &str, subtype: &str) -> bool {
    mime.ctype().eq_ignore_ascii_case(kind)
        && mime
            .subtype()
            .is_some_and(|value| value.eq_ignore_ascii_case(subtype))
}

fn crlf(bytes: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(bytes.len());
    for (index, &byte) in bytes.iter().enumerate() {
        if byte == b'\n' && (index == 0 || bytes[index - 1] != b'\r') {
            output.push(b'\r');
        }
        output.push(byte);
    }
    output
}

fn tool(name: &str, bundled: Option<&str>) -> Command {
    Command::new(
        bundled
            .filter(|path| Path::new(path).is_file())
            .unwrap_or(name),
    )
}

fn gpg() -> Command {
    let mut command = tool("gpg", option_env!("AGENTS_BUILD_GPG"));
    command.args([
        "--no-options",
        "--batch",
        "--no-tty",
        "--no-auto-key-retrieve",
        "--no-auto-key-import",
    ]);
    command
}

fn openssl() -> Command {
    tool("openssl", option_env!("AGENTS_BUILD_OPENSSL"))
}

fn run(command: &mut Command) -> Result<Output> {
    command.stdin(Stdio::null()).output().with_context(|| {
        format!(
            "execute mail signature tool {}",
            command.get_program().to_string_lossy()
        )
    })
}
