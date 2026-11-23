//! Configurable values: MiniJinja text, or JavaScript evaluated in a sandboxed QuickJS.

use anyhow::{Result, anyhow, bail};
use minijinja::{AutoEscape, Environment, UndefinedBehavior};
use rquickjs::{CatchResultExt, Context, Runtime};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    cell::{Cell, OnceCell},
    rc::Rc,
    time::{Duration, Instant},
};

/// MiniJinja text, or `{"js": "..."}`.
#[derive(Clone, Deserialize, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum Template {
    Text(String),
    Js(Script),
}

/// JavaScript run in QuickJS with no I/O. The template context is exposed as
/// global variables; the value of the last statement is the result. Output hooks
/// keep `globalThis` for the whole run; other scripts start from a fresh context.
#[derive(Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Script {
    js: String,
}

/// Renders templates against one context.
pub struct Renderer {
    jinja: Environment<'static>,
    context: minijinja::Value,
    globals: Value,
    js: OnceCell<Sandbox>,
}

impl Renderer {
    pub fn new(context: impl Serialize) -> Result<Self> {
        let globals = serde_json::to_value(context)?;
        let mut jinja = Environment::new();
        jinja.set_undefined_behavior(UndefinedBehavior::Strict);
        jinja.set_auto_escape_callback(|_| AutoEscape::None);
        jinja.set_keep_trailing_newline(true);
        Ok(Self {
            jinja,
            context: minijinja::Value::from_serialize(&globals),
            globals,
            js: OnceCell::new(),
        })
    }

    pub fn value(&self, template: &Template) -> Result<Value> {
        match template {
            Template::Text(text) => Ok(Value::String(self.jinja.render_str(text, &self.context)?)),
            Template::Js(script) => self.script(script),
        }
    }

    pub fn string(&self, template: &Template) -> Result<String> {
        match self.value(template)? {
            Value::String(text) => Ok(text),
            other => bail!("expected a string, got {other}"),
        }
    }

    pub fn script(&self, script: &Script) -> Result<Value> {
        if self.js.get().is_none() {
            let _ = self.js.set(Sandbox::new()?);
        }
        self.js
            .get()
            .expect("initialized")
            .eval(script, &self.globals)
    }
}

/// A QuickJS runtime with memory, stack, and time limits; each evaluation gets a fresh context.
pub struct Sandbox {
    runtime: Runtime,
    deadline: Rc<Cell<Instant>>,
}

impl Sandbox {
    const MEMORY: usize = 64 << 20;
    const STACK: usize = 1 << 20;
    const TIME: Duration = Duration::from_secs(1);

    pub fn new() -> Result<Self> {
        let runtime = Runtime::new()?;
        runtime.set_memory_limit(Self::MEMORY);
        runtime.set_max_stack_size(Self::STACK);
        let deadline = Rc::new(Cell::new(Instant::now()));
        let watched = Rc::clone(&deadline);
        runtime.set_interrupt_handler(Some(Box::new(move || Instant::now() > watched.get())));
        Ok(Self { runtime, deadline })
    }

    pub fn eval(&self, script: &Script, globals: &Value) -> Result<Value> {
        self.scope(globals)?.eval(script, &Value::Null)
    }

    /// A context that keeps its globals across evaluations, for stateful scripts.
    pub fn scope(&self, globals: &Value) -> Result<Scope<'_>> {
        let scope = Scope {
            sandbox: self,
            context: Context::full(&self.runtime)?,
        };
        scope.define(globals)?;
        Ok(scope)
    }
}

pub struct Scope<'a> {
    sandbox: &'a Sandbox,
    context: Context,
}

impl Scope<'_> {
    /// Defines `globals`, then evaluates `script`.
    pub fn eval(&self, script: &Script, globals: &Value) -> Result<Value> {
        self.define(globals)?;
        self.sandbox.deadline.set(Instant::now() + Sandbox::TIME);
        self.context.with(|ctx| {
            // A block keeps `let` and `const` local to one evaluation.
            let result: rquickjs::Value = ctx
                .eval(format!("{{\n{}\n}}", script.js))
                .catch(&ctx)
                .map_err(|error| anyhow!("JavaScript: {error}"))?;
            Ok(match ctx.json_stringify(result)? {
                Some(json) => serde_json::from_str(&json.to_string()?)?,
                None => Value::Null,
            })
        })
    }

    fn define(&self, globals: &Value) -> Result<()> {
        let Value::Object(globals) = globals else {
            return Ok(());
        };
        self.context.with(|ctx| {
            let scope = ctx.globals();
            for (name, value) in globals {
                scope.set(
                    name.as_str(),
                    ctx.json_parse(serde_json::to_string(value)?)?,
                )?;
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{Renderer, Sandbox, Script, Template};
    use serde_json::{Value, json};

    fn js(code: &str) -> Template {
        serde_json::from_value(json!({ "js": code })).unwrap()
    }

    #[test]
    fn scripts_see_the_context_and_return_json() {
        let renderer = Renderer::new(json!({"agent": {"id": "a"}, "items": [1, 2]})).unwrap();
        let value = renderer
            .value(&js("const n = items.length; ({id: agent.id, n})"))
            .unwrap();
        assert_eq!(value, json!({"id": "a", "n": 2}));
        assert_eq!(renderer.value(&js("undefined")).unwrap(), Value::Null);
    }

    #[test]
    fn scopes_keep_state_between_evaluations() {
        let sandbox = Sandbox::new().unwrap();
        let scope = sandbox.scope(&json!({"base": 10})).unwrap();
        let script: Script = serde_json::from_value(json!({
            "js": "const next = (globalThis.total ?? base) + n; globalThis.total = next; next"
        }))
        .unwrap();
        assert_eq!(scope.eval(&script, &json!({"n": 1})).unwrap(), json!(11));
        assert_eq!(scope.eval(&script, &json!({"n": 2})).unwrap(), json!(13));
    }

    #[test]
    fn scripts_are_isolated_and_bounded() {
        let renderer = Renderer::new(json!({})).unwrap();
        renderer.value(&js("globalThis.leak = 1")).unwrap();
        assert_eq!(
            renderer.value(&js("typeof leak")).unwrap(),
            json!("undefined")
        );
        for code in [
            "while (true) {}",
            "'x'.repeat(1 << 27)",
            "(function f() { f() })()",
            "throw new Error('boom')",
            "typeof require === 'undefined' && typeof std === 'undefined' ? null.x : 0",
        ] {
            assert!(renderer.value(&js(code)).is_err(), "{code}");
        }
    }
}
