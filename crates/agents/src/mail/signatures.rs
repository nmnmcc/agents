use super::retry;
use crate::types::MailAddress;
use anyhow::{Context, Result, bail, ensure};
use mail_parser::{ContentType, MessageParser, MimeHeaders, PartType};
use schemars::JsonSchema;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

pub(super) const LIMIT: usize = 8 * 1024 * 1024;

#[derive(Deserialize, JsonSchema)]
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
    /// Seconds after signing that mail is still accepted. Older mail is rejected,
    /// which bounds how long receipts are kept to detect replays.
    #[serde(default = "max_age_default")]
    #[schemars(range(min = 1))]
    max_age: u64,
}

impl Default for Signatures {
    fn default() -> Self {
        Self {
            openpgp_keyring: None,
            trusted: Vec::new(),
            signing: BTreeMap::new(),
            max_age: max_age_default(),
        }
    }
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

const fn max_age_default() -> u64 {
    7 * 24 * 60 * 60
}

/// Clock skew tolerated for signatures from the future.
const SKEW: u64 = 300;

/// Headers copied into the signed entity and preferred over the outer headers.
pub(super) const PROTECTED: &[&str] = &[
    "From",
    "Reply-To",
    "To",
    "Cc",
    "Subject",
    "Date",
    "Message-ID",
    "In-Reply-To",
    "References",
];

/// A message whose signature is valid and made by a pinned signer.
pub(super) struct Verified {
    pub signer: MailAddress,
    /// The signed MIME entity, including its own (possibly protected) headers.
    pub entity: Vec<u8>,
    /// Unix time the signature was made.
    pub signed_at: u64,
    /// Identifies this signature, independent of any unsigned outer headers.
    pub receipt: [u8; 32],
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
        ensure!(self.max_age > 0, "signatures.max_age must be positive");
        Ok(())
    }

    /// Verifies a standard OpenPGP/MIME or S/MIME signature by a pinned signer.
    /// Returns why a message is not accepted.
    pub fn verify(&self, root: &Path, raw: &[u8]) -> Result<Verified> {
        ensure!(raw.len() <= LIMIT, "mail exceeds size limit");
        let message = MessageParser::default()
            .parse(raw)
            .context("invalid RFC 5322 mail")?;
        let part = message.parts.first().context("empty mail")?;
        // Reject ambiguous MIME declarations instead of letting different verifiers pick one.
        ensure!(
            part.headers
                .iter()
                .filter(|header| header.name.as_str().eq_ignore_ascii_case("Content-Type"))
                .count()
                <= 1,
            "duplicate Content-Type header"
        );
        let mime = part.content_type();
        let is = |kind, subtype| mime.is_some_and(|m| content_type(m, kind, subtype));
        let protocol = mime.and_then(|m| m.attribute("protocol"));
        let uses = |expected: &str| protocol.is_some_and(|v| v.eq_ignore_ascii_case(expected));
        let signed = is("multipart", "signed");
        let verified = if is("application", "pkcs7-mime")
            || is("application", "x-pkcs7-mime")
            || (signed
                && (uses("application/pkcs7-signature") || uses("application/x-pkcs7-signature")))
        {
            self.verify_smime(raw)?
        } else if signed && uses("application/pgp-signature") {
            let PartType::Multipart(children) = &part.body else {
                bail!("malformed multipart/signed mail");
            };
            let [entity, signature] = children.as_slice() else {
                bail!("multipart/signed mail must have two parts");
            };
            let (Some(entity), Some(signature)) = (
                message.parts.get(*entity as usize),
                message.parts.get(*signature as usize),
            ) else {
                bail!("malformed multipart/signed mail");
            };
            ensure!(
                signature.content_type().is_some_and(|m| content_type(
                    m,
                    "application",
                    "pgp-signature"
                )),
                "second multipart/signed part is not an OpenPGP signature"
            );
            let entity = raw
                .get(entity.raw_header_offset() as usize..entity.raw_end_offset() as usize)
                .context("malformed multipart/signed mail")?;
            self.verify_pgp(root, &crlf(entity), signature.contents())?
        } else {
            bail!("mail is not signed with OpenPGP/MIME or S/MIME");
        };
        self.check_age(verified.signed_at)?;
        Ok(verified)
    }

    /// Seconds a signature remains acceptable, and so must be remembered to reject replays.
    pub fn max_age(&self) -> u64 {
        self.max_age
    }

    /// Rejects mail signed at `signed_at` that is too old or from the future.
    pub fn check_age(&self, signed_at: u64) -> Result<()> {
        let now = retry::now()?;
        ensure!(
            signed_at.saturating_add(self.max_age) >= now,
            "signature is older than signatures.max_age"
        );
        ensure!(
            signed_at <= now.saturating_add(SKEW),
            "signature is from the future"
        );
        Ok(())
    }

    fn identity(&self, method: Method, fingerprint: &str) -> Result<MailAddress> {
        let fingerprint = normalize(fingerprint);
        self.trusted
            .iter()
            .find(|entry| entry.method == method && normalize(&entry.fingerprint) == fingerprint)
            .map(|entry| entry.address.clone())
            .context("signer is not trusted")
    }

    fn verify_pgp(&self, root: &Path, entity: &[u8], signature: &[u8]) -> Result<Verified> {
        let keyring = self
            .openpgp_keyring
            .as_ref()
            .context("no OpenPGP keyring is configured")?;
        let keyring =
            fs::canonicalize(root.join(keyring)).context("open OpenPGP public keyring")?;
        let dir = tempfile::tempdir()?;
        let input = dir.path().join("input");
        let detached = dir.path().join("signature");
        fs::write(&input, entity)?;
        fs::write(&detached, signature)?;
        let result = run(gpg()
            .arg("--homedir")
            .arg(dir.path())
            .args(["--lock-never", "--no-default-keyring", "--keyring"])
            .arg(keyring)
            .args(["--status-fd", "1", "--verify"])
            .arg(detached)
            .arg(input))?;
        ensure!(result.status.success(), "invalid OpenPGP signature");
        let status = String::from_utf8_lossy(&result.stdout);
        let (fingerprint, signed_at) =
            pgp_signature(&status).context("invalid OpenPGP signature")?;
        let signer = self.identity(Method::Openpgp, fingerprint)?;
        Ok(Verified {
            receipt: receipt(fingerprint, signed_at, entity),
            signer,
            entity: entity.to_vec(),
            signed_at,
        })
    }

    fn verify_smime(&self, raw: &[u8]) -> Result<Verified> {
        let dir = tempfile::tempdir()?;
        let input = dir.path().join("input");
        let cert = dir.path().join("signer.pem");
        fs::write(&input, raw)?;
        // The leaf is pinned below; do not rely on the public CA store or the From header.
        // -noverify skips certificate chain validation, NOT the CMS content/signature checks.
        let verified = run(openssl()
            .args(["cms", "-verify", "-inform", "SMIME", "-noverify", "-in"])
            .arg(&input)
            .arg("-signer")
            .arg(&cert))?;
        ensure!(verified.status.success(), "invalid S/MIME signature");
        let cert_bytes = fs::read(&cert)?;
        ensure!(
            cert_bytes
                .split(|&byte| byte == b'\n')
                .filter(|line| {
                    line.strip_suffix(b"\r").unwrap_or(line) == b"-----BEGIN CERTIFICATE-----"
                })
                .count()
                == 1,
            "S/MIME mail must have exactly one signer"
        );
        let details = run(openssl()
            .args([
                "x509",
                "-noout",
                "-sha256",
                "-fingerprint",
                "-checkend",
                "0",
                "-in",
            ])
            .arg(&cert))?;
        ensure!(details.status.success(), "S/MIME certificate has expired");
        let details = String::from_utf8_lossy(&details.stdout);
        let fingerprint = details
            .lines()
            .find_map(|line| line.split_once('=').map(|(_, value)| value.trim()))
            .context("read S/MIME certificate fingerprint")?;
        let signer = self.identity(Method::Smime, fingerprint)?;
        let printed = run(openssl()
            .args([
                "cms", "-cmsout", "-print", "-noout", "-inform", "SMIME", "-in",
            ])
            .arg(&input))?;
        ensure!(printed.status.success(), "read S/MIME signed attributes");
        let entity = crlf(&verified.stdout);
        let signed_at = match smime_signing_time(&String::from_utf8_lossy(&printed.stdout)) {
            Some(time) => time,
            // Without a signingTime attribute, rely on a protected Date header.
            None => MessageParser::default()
                .parse_headers(&entity)
                .and_then(|headers| headers.date().map(|date| date.to_timestamp()))
                .and_then(|time| u64::try_from(time).ok())
                .context("S/MIME signature has no signing time or protected Date")?,
        };
        Ok(Verified {
            receipt: receipt(fingerprint, signed_at, &entity),
            signer,
            entity,
            signed_at,
        })
    }

    /// Signs a queued message as standard multipart/signed mail. The signed entity carries
    /// the protected headers; the outer headers are sent from the shared SMTP address.
    pub fn sign(
        &self,
        root: &Path,
        agent: &MailAddress,
        from: &MailAddress,
        raw: &[u8],
    ) -> Result<Vec<u8>> {
        let identity = |address: &MailAddress| {
            self.signing
                .iter()
                .find(|(key, _)| key.matches(address))
                .map(|(_, signing)| signing)
        };
        let (signer, signing) = match identity(agent) {
            Some(signing) => (agent, signing),
            None => (
                from,
                identity(from).with_context(|| {
                    format!("no SMTP signing identity configured for {agent} or {from}")
                })?,
            ),
        };
        let message = MessageParser::default()
            .parse(raw)
            .context("invalid queued mail")?;
        let part = message.parts.first().context("empty queued mail")?;
        let body = raw
            .get(part.raw_body_offset() as usize..)
            .context("invalid queued mail")?;
        // Replies go to the agent; From names the signer, which receivers attribute mail to.
        let mut headers = format!("From: {signer}\r\n");
        if !agent.matches(signer) {
            headers.push_str(&format!("Reply-To: {agent}\r\n"));
        }
        let mut content = Vec::new();
        for (name, value) in message.headers_raw() {
            let line = format!("{name}: {}", value.trim_start());
            let line = if line.ends_with('\n') {
                line
            } else {
                line + "\r\n"
            };
            let lower = name.to_ascii_lowercase();
            if PROTECTED
                .iter()
                .any(|protected| protected.eq_ignore_ascii_case(name))
                && !matches!(lower.as_str(), "from" | "reply-to")
            {
                headers.push_str(&line);
            } else if lower == "content-type" {
                content.push(format!("{}; protected-headers=\"v1\"\r\n", line.trim_end()));
            } else if lower == "content-transfer-encoding" {
                content.push(line);
            }
        }
        let mut entity = content.concat().into_bytes();
        entity.extend_from_slice(headers.as_bytes());
        entity.extend_from_slice(b"\r\n");
        entity.extend_from_slice(body);
        let entity = crlf(&entity);
        let dir = tempfile::tempdir()?;
        let input = dir.path().join("entity");
        fs::write(&input, &entity)?;
        let signed = match signing {
            Signing::Openpgp { fingerprint, home } => {
                let result = run(gpg()
                    .arg("--homedir")
                    .arg(root.join(home))
                    .args([
                        "--local-user",
                        fingerprint,
                        "--digest-algo",
                        "SHA256",
                        "--armor",
                        "--output",
                        "-",
                        "--detach-sign",
                    ])
                    .arg(&input))?;
                ensure!(result.status.success(), "OpenPGP signing failed");
                let boundary = format!("=_{}", uuid::Uuid::new_v4().simple());
                let mut signed = format!(
                    "Content-Type: multipart/signed; micalg=pgp-sha256;\r\n \
                     protocol=\"application/pgp-signature\"; boundary=\"{boundary}\"\r\n\r\n\
                     --{boundary}\r\n"
                )
                .into_bytes();
                signed.extend_from_slice(&entity);
                signed.extend_from_slice(
                    format!(
                        "\r\n--{boundary}\r\n\
                         Content-Type: application/pgp-signature; name=\"signature.asc\"\r\n\
                         Content-Disposition: attachment; filename=\"signature.asc\"\r\n\r\n"
                    )
                    .as_bytes(),
                );
                signed.extend_from_slice(&result.stdout);
                signed.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
                signed
            }
            Signing::Smime { certificate, key } => {
                let result = run(openssl()
                    .args(["cms", "-sign", "-binary", "-outform", "SMIME", "-in"])
                    .arg(&input)
                    .arg("-signer")
                    .arg(root.join(certificate))
                    .arg("-inkey")
                    .arg(root.join(key)))?;
                ensure!(result.status.success(), "S/MIME signing failed");
                // openssl writes MIME-Version and the multipart/signed headers itself.
                result.stdout
            }
        };
        let mut wire = format!("From: {from}\r\n").into_bytes();
        if !agent.matches(from) {
            wire.extend_from_slice(format!("Reply-To: {agent}\r\n").as_bytes());
        }
        for (name, value) in message.headers_raw() {
            if PROTECTED
                .iter()
                .any(|protected| protected.eq_ignore_ascii_case(name))
                && !matches!(name.to_ascii_lowercase().as_str(), "from" | "reply-to")
            {
                wire.extend_from_slice(format!("{name}: {}", value.trim_start()).as_bytes());
                if !wire.ends_with(b"\n") {
                    wire.extend_from_slice(b"\r\n");
                }
            }
        }
        if matches!(signing, Signing::Openpgp { .. }) {
            wire.extend_from_slice(b"MIME-Version: 1.0\r\n");
        }
        wire.extend_from_slice(&signed);
        let wire = crlf(&wire);
        ensure!(wire.len() <= LIMIT, "signed mail exceeds size limit");
        Ok(wire)
    }
}

fn normalize(value: &str) -> String {
    value.replace(':', "").to_ascii_uppercase()
}

fn receipt(fingerprint: &str, signed_at: u64, entity: &[u8]) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(normalize(fingerprint));
    digest.update(signed_at.to_be_bytes());
    digest.update(entity);
    digest.finalize().into()
}

/// Reads `openssl cms -print` output: "signingTime" is followed by a UTCTIME or
/// GENERALIZEDTIME line such as "UTCTIME:Nov 23 10:00:00 2026 GMT".
fn smime_signing_time(printed: &str) -> Option<u64> {
    let mut lines = printed
        .lines()
        .skip_while(|line| !line.contains("signingTime"));
    lines.next()?;
    let value = lines
        .take(3)
        .find_map(|line| line.trim().split_once("TIME:").map(|(_, value)| value))?;
    let mut fields = value.split_whitespace();
    let month = fields.next()?;
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ]
    .iter()
    .position(|name| *name == month)? as u64
        + 1;
    let day: u64 = fields.next()?.parse().ok()?;
    let mut clock = fields.next()?.split(':').map(str::parse::<u64>);
    let (hour, minute, second) = (
        clock.next()?.ok()?,
        clock.next()?.ok()?,
        clock.next()?.ok()?,
    );
    let year: i64 = fields.next()?.parse().ok()?;
    if fields.next() != Some("GMT") || !(1..=31).contains(&day) || hour > 23 || minute > 59 {
        return None;
    }
    // Days from civil date (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = month as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400)
        .ok()?
        .checked_add(hour * 3600 + minute * 60 + second.min(60))
}

/// The primary-key fingerprint and signature time of exactly one valid signature.
fn pgp_signature(status: &str) -> Option<(&str, u64)> {
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
                fingerprint = Some((
                    *fields.get(10).or_else(|| fields.get(1))?,
                    fields.get(3)?.parse().ok()?,
                ));
            }
            "BADSIG" | "ERRSIG" | "EXPSIG" | "EXPKEYSIG" | "REVKEYSIG" | "NO_PUBKEY"
            | "KEYEXPIRED" | "SIGEXPIRED" | "FAILURE" | "DECRYPTION_OKAY" => return None,
            _ => {}
        }
        Some(())
    })?;
    fingerprint
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_cms_signing_time() {
        let printed = "        object: signingTime (1.2.840.113549.1.9.5)\n\
                       \x20       set:\n\
                       \x20         UTCTIME:Nov 23 10:00:05 2026 GMT\n";
        assert_eq!(smime_signing_time(printed), Some(1_795_428_005));
        let generalized = printed.replace("UTCTIME:Nov 23", "GENERALIZEDTIME:Feb 29");
        assert_eq!(
            smime_signing_time(&generalized.replace("2026", "2028")),
            Some(1_835_431_205)
        );
        assert_eq!(smime_signing_time("no signing time"), None);
        assert_eq!(smime_signing_time(&printed.replace("GMT", "UTC")), None);
    }

    #[test]
    fn requires_exactly_one_valid_pgp_signature() {
        let valid = "[GNUPG:] NEWSIG\n[GNUPG:] GOODSIG 0123 Someone\n\
                     [GNUPG:] VALIDSIG SUBKEY 2026-11-23 1795428005 0 4 0 22 10 00 PRIMARY\n";
        assert_eq!(pgp_signature(valid), Some(("PRIMARY", 1_795_428_005)));
        assert_eq!(pgp_signature(&format!("{valid}{valid}")), None);
        assert_eq!(pgp_signature(&valid.replace(" 10 00 ", " 2 00 ")), None);
        assert_eq!(
            pgp_signature(&format!("{valid}[GNUPG:] EXPKEYSIG 0123\n")),
            None
        );
        assert_eq!(pgp_signature("[GNUPG:] NEWSIG\n"), None);
    }

    #[test]
    fn receipts_cover_signer_time_and_content() {
        let base = receipt("ab:cd", 1, b"entity");
        assert_eq!(base, receipt("ABCD", 1, b"entity"));
        assert_ne!(base, receipt("ABCE", 1, b"entity"));
        assert_ne!(base, receipt("ABCD", 2, b"entity"));
        assert_ne!(base, receipt("ABCD", 1, b"entity2"));
    }
}
