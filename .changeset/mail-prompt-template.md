---
default: minor
---

# Customizable mail prompt

The prompt an agent receives for a new message is a template, set in `mail.prompt` or per agent in `agents.<id>.mail.prompt`. Its context has `agent`, `address` and `mail`, and the `json` filter prints indented JSON without HTML escaping.
