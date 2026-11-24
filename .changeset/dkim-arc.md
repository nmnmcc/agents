---
default: minor
---

# Authenticate mail by a trusted provider's DKIM and ARC verdicts

`mail.transport.dkim.authserv_ids` accepts unsigned mail when a trusted mail provider reports an aligned `dkim=pass` in the topmost `Authentication-Results`, and the sender is in `mail.trust`. `arc_sealers` accepts DKIM verdicts recorded by trusted forwarders when the provider reports `arc=pass`. Only headers covered by the DKIM signature are used.

The agent prompt no longer describes how the sender was authenticated; which mail is accepted is decided by configuration alone.
