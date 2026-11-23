# Changelog

## 0.2.0 (2026-11-23)

### Breaking Changes

#### Authenticate mail with OpenPGP or S/MIME without a gateway

Poll an ordinary IMAP inbox and verify task signatures against pinned identities.
SMTP delivery signs tasks per recipient. Unsigned ingress is disabled by
default and may be explicitly enabled without granting reply or thread identity.

The old mail gateway headers and `mail deliver --authenticated-from` are no longer
accepted. See the signed mail setup in the README to migrate.

## 0.1.0

### Features

#### Initial release

Run portable project agents through existing harness CLIs.
