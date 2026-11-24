---
default: patch
---

# Document template fields uniformly in the JSON Schema

Every template and script field in `agents.schema.json` now states its context and expected result in the same `Template context:` / `Result:` form, and the `Template` and `Script` definitions describe the MiniJinja and JavaScript syntax.
