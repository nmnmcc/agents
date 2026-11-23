---
default: minor
---

# Share one SMTP sender across agents

Set `mail.transport.smtp.from` to a provider-authorized address while
using distinct `+tag` addresses for agent routing and replies. Agents can
share the sender's signing key or keep their own signing identities.
