//! Authenticates mail by the DKIM and ARC verdicts of a trusted mail provider.
//!
//! The provider verifies DKIM and ARC when it receives mail and records the outcome in an
//! `Authentication-Results` header (RFC 8601) named by its authserv-id. Such a provider removes
//! headers that claim its authserv-id from incoming mail, so its verdict cannot be forged.

use anyhow::{Context, Result, bail, ensure};
use mail_parser::Message;
use schemars::JsonSchema;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct Dkim {
    /// authserv-ids your mail provider writes into `Authentication-Results`, such as
    /// `mx.google.com`. Mail with an aligned DKIM pass from one of them is authenticated.
    /// The provider must remove headers claiming these ids from incoming mail (RFC 8601).
    #[serde(default)]
    authserv_ids: Vec<String>,
    /// Domains whose ARC seal is trusted to report DKIM results of mail they forward,
    /// such as mailing lists whose changes break the original DKIM signature.
    #[serde(default)]
    arc_sealers: Vec<String>,
}

/// A message whose From domain is authenticated by DKIM.
#[derive(Debug)]
pub(super) struct Authenticated {
    /// The DKIM signing domain aligned with the From domain.
    pub domain: String,
    /// The trusted ARC sealer that reported the DKIM result, for forwarded mail.
    pub sealer: Option<String>,
    /// Lowercase names of the headers the authenticating signature covers.
    pub covered: BTreeSet<String>,
    /// Unix time the authenticating signature was made, when it says so.
    pub signed_at: Option<u64>,
    /// The signature value, unique to this signature.
    pub signature: String,
}

impl Authenticated {
    pub fn describe(&self) -> String {
        match &self.sealer {
            None => format!("dkim {}", self.domain),
            Some(sealer) => format!("arc {sealer} dkim {}", self.domain),
        }
    }
}

/// Headers that must occur at most once, since DKIM verifiers and mail readers may pick
/// different instances of a repeated header.
const SINGLE: &[&str] = &[
    "From",
    "Sender",
    "Reply-To",
    "To",
    "Cc",
    "Subject",
    "Date",
    "Message-ID",
    "In-Reply-To",
    "References",
];

impl Dkim {
    pub fn check(&self) -> Result<()> {
        for id in &self.authserv_ids {
            ensure!(
                !id.trim().is_empty() && !id.contains([';', ' ', '(']),
                "invalid DKIM authserv-id {id:?}"
            );
        }
        for domain in &self.arc_sealers {
            ensure!(
                domain.contains('.') && !domain.contains(['@', ' ', ';']),
                "invalid ARC sealer domain {domain:?}"
            );
        }
        ensure!(
            self.arc_sealers.is_empty() || self.enabled(),
            "dkim.arc_sealers requires dkim.authserv_ids"
        );
        Ok(())
    }

    pub fn enabled(&self) -> bool {
        !self.authserv_ids.is_empty()
    }

    fn trusts(&self, id: &str) -> bool {
        self.authserv_ids
            .iter()
            .any(|known| known.eq_ignore_ascii_case(id))
    }

    /// Authenticates the From domain `from` of `message` from the provider's verdicts.
    pub fn authenticate(&self, message: &Message<'_>, from: &str) -> Result<Authenticated> {
        ensure!(self.enabled(), "DKIM authentication is not configured");
        for name in SINGLE {
            ensure!(
                headers(message, name).len() <= 1,
                "mail has more than one {name} header"
            );
        }
        let from = domain(from);
        // The provider adds its verdict on top of everything it received.
        let all = headers(message, "Authentication-Results");
        let verdict = all
            .first()
            .and_then(|value| results(value))
            .filter(|results| self.trusts(&results.authserv_id))
            .context("mail has no Authentication-Results from a trusted provider on top")?;
        ensure!(
            all.iter()
                .skip(1)
                .filter_map(|value| results(value))
                .all(|results| !self.trusts(&results.authserv_id)),
            "mail has more than one Authentication-Results from a trusted provider"
        );

        for entry in verdict.passed("dkim") {
            let Some(signer) = entry.dkim_domain() else {
                continue;
            };
            if !aligned(&signer, &from) {
                continue;
            }
            let candidates: Vec<_> = headers(message, "DKIM-Signature")
                .iter()
                .map(|value| tags(value))
                .filter(|tags| entry.identifies(tags, &signer))
                .collect();
            if let Some(authenticated) = signature(&candidates, &signer, None)? {
                return Ok(authenticated);
            }
        }

        if !self.arc_sealers.is_empty() && verdict.passed("arc").next().is_some() {
            return self.forwarded(message, &from);
        }
        bail!("no DKIM signature aligned with {from} passed at the trusted provider")
    }

    /// Takes the DKIM result reported by the latest ARC sealer, if it is trusted.
    /// The provider's `arc=pass` covers every seal and the latest message signature.
    fn forwarded(&self, message: &Message<'_>, from: &str) -> Result<Authenticated> {
        let instance = |name| -> Vec<(u32, String)> {
            headers(message, name)
                .iter()
                .filter_map(|value| {
                    let (i, rest) = instance(value)?;
                    Some((i, rest.to_owned()))
                })
                .collect()
        };
        let seals = instance("ARC-Seal");
        let latest = seals
            .iter()
            .map(|(i, _)| *i)
            .max()
            .context("mail has no ARC seal")?;
        let only = |sets: &[(u32, String)]| -> Result<String> {
            let [(_, value)] = sets
                .iter()
                .filter(|(i, _)| *i == latest)
                .cloned()
                .collect::<Vec<_>>()
                .try_into()
                .ok()
                .context("ambiguous ARC set")?;
            Ok(value)
        };
        let seal = tags(&only(&seals)?);
        let signature_tags = tags(&only(&instance("ARC-Message-Signature"))?);
        let assessment = results(&only(&instance("ARC-Authentication-Results"))?)
            .context("invalid ARC-Authentication-Results")?;
        let sealer = seal
            .get("d")
            .map(|d| domain(d))
            .context("ARC seal has no domain")?;
        ensure!(
            self.arc_sealers
                .iter()
                .any(|trusted| aligned(&domain(trusted), &sealer)),
            "mail was last sealed by {sealer}, which is not a trusted ARC sealer"
        );
        let signer = assessment
            .passed("dkim")
            .filter_map(Entry::dkim_domain)
            .find(|signer| aligned(signer, from))
            .with_context(|| {
                format!("ARC sealer {sealer} reports no aligned DKIM pass for {from}")
            })?;
        let signed_at = seal.get("t").and_then(|t| t.parse().ok());
        let mut authenticated = signature(&[signature_tags], &sealer, signed_at)?
            .context("invalid ARC message signature")?;
        authenticated.domain = signer;
        authenticated.sealer = Some(sealer);
        Ok(authenticated)
    }
}

/// Combines the matching signatures into what they all authenticate.
fn signature(
    candidates: &[BTreeMap<String, String>],
    domain: &str,
    signed_at: Option<u64>,
) -> Result<Option<Authenticated>> {
    let mut covered: Option<BTreeSet<String>> = None;
    let mut times = Vec::new();
    let mut values = Vec::new();
    for tags in candidates {
        // A body length limit lets anyone append unsigned content.
        ensure!(
            !tags.contains_key("l"),
            "signature covers only part of the body"
        );
        let names: BTreeSet<String> = tags
            .get("h")
            .map(|h| {
                h.split(':')
                    .map(|name| name.trim().to_ascii_lowercase())
                    .collect()
            })
            .unwrap_or_default();
        ensure!(names.contains("from"), "signature does not cover From");
        covered = Some(match covered {
            Some(known) => known.intersection(&names).cloned().collect(),
            None => names,
        });
        times.extend(tags.get("t").and_then(|t| t.parse::<u64>().ok()));
        values.push(tags.get("b").cloned().unwrap_or_default());
    }
    let Some(covered) = covered else {
        return Ok(None);
    };
    Ok(Some(Authenticated {
        domain: domain.to_owned(),
        sealer: None,
        covered,
        signed_at: signed_at.or_else(|| times.into_iter().max()),
        signature: values.join(" "),
    }))
}

struct Results {
    authserv_id: String,
    entries: Vec<Entry>,
}

impl Results {
    fn passed<'a>(&'a self, method: &'a str) -> impl Iterator<Item = &'a Entry> {
        self.entries
            .iter()
            .filter(move |entry| entry.method == method && entry.result == "pass")
    }
}

struct Entry {
    method: String,
    result: String,
    properties: BTreeMap<String, String>,
}

impl Entry {
    /// The signing domain from `header.d`, or else the domain of `header.i`.
    fn dkim_domain(&self) -> Option<String> {
        self.properties
            .get("header.d")
            .map(|d| domain(d))
            .or_else(|| {
                let identity = self.properties.get("header.i")?;
                Some(domain(
                    identity
                        .rsplit_once('@')
                        .map_or(identity.as_str(), |(_, d)| d),
                ))
            })
    }

    /// Whether `tags` is a DKIM signature this result can refer to.
    fn identifies(&self, tags: &BTreeMap<String, String>, signer: &str) -> bool {
        let Some(d) = tags.get("d") else {
            return false;
        };
        if !aligned(&domain(d), signer) {
            return false;
        }
        match self.properties.get("header.b") {
            Some(prefix) => tags
                .get("b")
                .is_some_and(|b| prefix.len() >= 8 && b.starts_with(prefix.as_str())),
            None => true,
        }
    }
}

/// Whether `signer` authenticates mail from `from`, using relaxed DMARC alignment
/// without the public suffix list: the same domain or a parent of it.
fn aligned(signer: &str, from: &str) -> bool {
    signer.contains('.') && (from == signer || from.ends_with(&format!(".{signer}")))
}

fn domain(value: &str) -> String {
    value.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// Raw, unfolded values of the headers named `name`, top first.
fn headers(message: &Message<'_>, name: &str) -> Vec<String> {
    message
        .headers_raw()
        .filter(|(header, _)| header.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.replace(['\r', '\n'], " "))
        .collect()
}

/// Splits `i=N; rest` from an ARC header.
fn instance(value: &str) -> Option<(u32, &str)> {
    let (first, rest) = value.split_once(';')?;
    let (key, i) = first.split_once('=')?;
    (key.trim().eq_ignore_ascii_case("i"))
        .then(|| i.trim().parse().ok())
        .flatten()
        .map(|i| (i, rest))
}

/// Parses a DKIM tag list (RFC 6376 §3.2). Whitespace inside values is removed.
fn tags(value: &str) -> BTreeMap<String, String> {
    value
        .split(';')
        .filter_map(|tag| {
            let (key, value) = tag.split_once('=')?;
            Some((
                key.trim().to_ascii_lowercase(),
                value.chars().filter(|c| !c.is_whitespace()).collect(),
            ))
        })
        .collect()
}

/// Parses an `Authentication-Results` value (RFC 8601 §2.2).
fn results(value: &str) -> Option<Results> {
    let text = uncomment(value);
    let mut statements = split(&text, ';').into_iter();
    let authserv_id = statements.next()?.split_whitespace().next()?.to_owned();
    let mut entries = Vec::new();
    for statement in statements {
        let mut words = split(&statement, ' ')
            .into_iter()
            .filter(|word| !word.is_empty());
        let Some((method, result)) = words.next().and_then(|word| {
            let (method, result) = word.split_once('=')?;
            let method = method.split('/').next().unwrap_or(method);
            Some((method.to_ascii_lowercase(), result.to_ascii_lowercase()))
        }) else {
            continue;
        };
        let properties = words
            .filter_map(|word| {
                let (key, value) = word.split_once('=')?;
                Some((key.to_ascii_lowercase(), unquote(value)))
            })
            .collect();
        entries.push(Entry {
            method,
            result,
            properties,
        });
    }
    Some(Results {
        authserv_id,
        entries,
    })
}

/// Removes RFC 5322 comments and whitespace around `=`, keeping quoted strings intact.
fn uncomment(value: &str) -> String {
    let mut output = String::new();
    let (mut depth, mut quoted, mut escaped) = (0_u32, false, false);
    for c in value.chars() {
        if escaped {
            escaped = false;
            if depth == 0 {
                output.push(c);
            }
            continue;
        }
        match c {
            '\\' => {
                escaped = true;
                if depth == 0 {
                    output.push(c);
                }
            }
            '"' if depth == 0 => {
                quoted = !quoted;
                output.push(c);
            }
            '(' if !quoted => depth += 1,
            ')' if !quoted && depth > 0 => {
                depth -= 1;
                output.push(' ');
            }
            _ if depth == 0 => output.push(if c.is_whitespace() { ' ' } else { c }),
            _ => {}
        }
    }
    let mut joined = String::new();
    for part in split(&output, '=') {
        if !joined.is_empty() {
            joined.truncate(joined.trim_end().len());
            joined.push('=');
        }
        joined.push_str(if joined.is_empty() {
            &part
        } else {
            part.trim_start()
        });
    }
    joined
}

/// Splits on `separator` outside quoted strings.
fn split(value: &str, separator: char) -> Vec<String> {
    let mut parts = vec![String::new()];
    let (mut quoted, mut escaped) = (false, false);
    for c in value.chars() {
        let current = parts.last_mut().expect("never empty");
        if escaped {
            escaped = false;
            current.push(c);
        } else if c == '\\' {
            escaped = true;
            current.push(c);
        } else if c == '"' {
            quoted = !quoted;
            current.push(c);
        } else if c == separator && !quoted {
            parts.push(String::new());
        } else {
            current.push(c);
        }
    }
    parts
        .into_iter()
        .map(|part| part.trim().to_owned())
        .collect()
}

fn unquote(value: &str) -> String {
    match value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
        Some(inner) => inner.replace("\\\"", "\"").replace("\\\\", "\\"),
        None => value.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::{Dkim, results};
    use mail_parser::MessageParser;

    fn dkim(arc: &[&str]) -> Dkim {
        Dkim {
            authserv_ids: vec!["mx.provider.test".into()],
            arc_sealers: arc.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    const SIGNATURE: &str = "DKIM-Signature: v=1; a=rsa-sha256; d=example.com; s=s1; t=1700000000;\r\n\th=from:to:subject:date:message-id; bh=AAAA; b=ABCDEFGHIJKLmnop\r\n";
    const BODY: &str = "From: Alice <alice@example.com>\r\nTo: agent+a@local.test\r\nCc: other@local.test\r\nSubject: hi\r\nDate: Tue, 14 Nov 2023 22:13:20 +0000\r\nMessage-ID: <m@example.com>\r\n\r\nbody\r\n";

    fn authenticate(dkim: &Dkim, headers: &str) -> anyhow::Result<super::Authenticated> {
        let raw = format!("{headers}{BODY}");
        let message = MessageParser::default().parse(raw.as_bytes()).unwrap();
        dkim.authenticate(&message, "example.com")
    }

    #[test]
    fn parses_authentication_results() {
        let parsed = results(
            "mx.google.com (v1);\r\n dkim=pass (2048-bit key) header.i=@example.com header.s=s1 header.b=\"ABCDEFGH\";\r\n spf=pass (google.com: domain of a@example.com) smtp.mailfrom=a@example.com;\r\n dmarc = pass (p=REJECT) header.from=example.com",
        )
        .unwrap();
        assert_eq!(parsed.authserv_id, "mx.google.com");
        let dkim = parsed.passed("dkim").next().unwrap();
        assert_eq!(dkim.dkim_domain().as_deref(), Some("example.com"));
        assert_eq!(dkim.properties["header.b"], "ABCDEFGH");
        assert_eq!(
            parsed.passed("dmarc").next().unwrap().properties["header.from"],
            "example.com"
        );
        assert!(
            results("mx.provider.test; none")
                .unwrap()
                .entries
                .is_empty()
        );
    }

    #[test]
    fn trusts_aligned_dkim_from_the_provider_on_top() {
        let verdict = "Authentication-Results: mx.provider.test; dkim=pass header.d=example.com header.b=ABCDEFGH\r\n";
        let ok = authenticate(&dkim(&[]), &format!("{verdict}{SIGNATURE}")).unwrap();
        assert_eq!(ok.describe(), "dkim example.com");
        assert_eq!(ok.signed_at, Some(1_700_000_000));
        assert!(ok.covered.contains("to") && !ok.covered.contains("cc"));

        // A forged verdict below the provider's, or none on top, is not trusted.
        let forged = "Authentication-Results: mx.provider.test; dkim=pass header.d=example.com\r\n";
        let error = authenticate(
            &dkim(&[]),
            &format!("Authentication-Results: mx.provider.test; dkim=fail header.d=example.com\r\n{forged}{SIGNATURE}"),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("more than one"));
        assert!(
            authenticate(
                &dkim(&[]),
                &format!("Authentication-Results: other.test; none\r\n{forged}{SIGNATURE}")
            )
            .is_err()
        );

        // A signature for another domain, a non-matching header.b, or l= is rejected.
        for (verdict, signature) in [
            ("dkim=pass header.d=evil.test", SIGNATURE.to_owned()),
            (
                "dkim=pass header.d=example.com header.b=ZZZZZZZZ",
                SIGNATURE.to_owned(),
            ),
            ("dkim=fail header.d=example.com", SIGNATURE.to_owned()),
            (
                "dkim=pass header.d=example.com",
                SIGNATURE.replace("bh=", "l=10; bh="),
            ),
        ] {
            let headers =
                format!("Authentication-Results: mx.provider.test; {verdict}\r\n{signature}");
            assert!(authenticate(&dkim(&[]), &headers).is_err(), "{verdict}");
        }

        // Repeated headers are ambiguous.
        let raw = format!("{verdict}{SIGNATURE}To: evil@local.test\r\n{BODY}");
        let message = MessageParser::default().parse(raw.as_bytes()).unwrap();
        assert!(dkim(&[]).authenticate(&message, "example.com").is_err());
    }

    #[test]
    fn trusts_dkim_reported_by_a_trusted_arc_sealer() {
        let arc = "Authentication-Results: mx.provider.test; dkim=fail header.d=example.com; dkim=pass header.d=lists.test; arc=pass\r\n\
            ARC-Seal: i=2; a=rsa-sha256; cv=pass; d=lists.test; s=a; t=1700000100; b=SEAL2\r\n\
            ARC-Message-Signature: i=2; a=rsa-sha256; d=lists.test; s=a; h=from:to:cc:subject:date:message-id; bh=X; b=AMS2\r\n\
            ARC-Authentication-Results: i=2; mx.lists.test; dkim=pass header.i=@example.com\r\n\
            ARC-Seal: i=1; a=rsa-sha256; cv=none; d=evil.test; s=a; b=SEAL1\r\n\
            ARC-Message-Signature: i=1; a=rsa-sha256; d=evil.test; s=a; h=from; bh=X; b=AMS1\r\n\
            ARC-Authentication-Results: i=1; evil.test; dkim=pass header.d=example.com\r\n";
        let ok = authenticate(&dkim(&["lists.test"]), &format!("{arc}{SIGNATURE}")).unwrap();
        assert_eq!(ok.describe(), "arc lists.test dkim example.com");
        assert_eq!(ok.signed_at, Some(1_700_000_100));
        assert!(ok.covered.contains("cc"));
        assert!(authenticate(&dkim(&["evil.test"]), &format!("{arc}{SIGNATURE}")).is_err());
        assert!(authenticate(&dkim(&[]), &format!("{arc}{SIGNATURE}")).is_err());
        let failed = arc.replace("arc=pass", "arc=fail");
        assert!(authenticate(&dkim(&["lists.test"]), &format!("{failed}{SIGNATURE}")).is_err());
    }
}
