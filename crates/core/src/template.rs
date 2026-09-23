use minijinja::Environment;
use serde_json::{Map, Value};

use crate::CoreError;

/// `{{ }}` rendering over JSON documents.
///
/// Typing rule: a string that is EXACTLY one expression (`"{{ order_id }}"`)
/// substitutes the native JSON value — a captured number stays a number,
/// which is what makes DB matchers work. Mixed strings render to text.
pub struct TemplateEngine {
    env: Environment<'static>,
    ctx: Value,
}

impl TemplateEngine {
    /// `anchor_unix_ms` freezes `now()` per test so `!near-now` compares
    /// against a stable point.
    pub fn new(ctx: Value, anchor_unix_ms: i64) -> Self {
        let mut env = Environment::new();
        env.add_function("uuid", || uuid::Uuid::new_v4().to_string());
        let anchor = anchor_unix_ms;
        env.add_function("now", move || {
            chrono::DateTime::from_timestamp_millis(anchor)
                .unwrap_or_default()
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        });
        env.add_function("random_int", |a: i64, b: i64| {
            a + (rand::random::<u64>() % (b.saturating_sub(a).max(1) as u64)) as i64
        });
        Self { env, ctx }
    }

    pub fn context(&self) -> &Value {
        &self.ctx
    }

    pub fn set(&mut self, namespace: &str, key: &str, value: Value) {
        let root = self.ctx.as_object_mut().expect("template ctx is an object");
        let ns = root
            .entry(namespace.to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if let Some(ns) = ns.as_object_mut() {
            ns.insert(key.to_string(), value);
        }
    }

    /// Flat capture shorthand: `{{ order_id }}` next to
    /// `{{ steps.create.captures.order_id }}`.
    pub fn set_flat(&mut self, key: &str, value: Value) {
        let root = self.ctx.as_object_mut().expect("template ctx is an object");
        root.insert(key.to_string(), value);
    }

    pub fn render_str(&self, s: &str) -> Result<String, CoreError> {
        self.env
            .render_str(s, &self.ctx)
            .map_err(|e| CoreError::Template(format!("`{s}`: {e}")))
    }

    pub fn render_value(&self, v: &Value) -> Result<Value, CoreError> {
        match v {
            Value::String(s) => self.render_string_value(s),
            Value::Array(items) => Ok(Value::Array(
                items.iter().map(|i| self.render_value(i)).collect::<Result<_, _>>()?,
            )),
            Value::Object(m) => {
                let mut out = Map::with_capacity(m.len());
                for (k, val) in m {
                    out.insert(k.clone(), self.render_value(val)?);
                }
                Ok(Value::Object(out))
            }
            other => Ok(other.clone()),
        }
    }

    fn render_string_value(&self, s: &str) -> Result<Value, CoreError> {
        if !s.contains("{{") {
            return Ok(Value::String(s.to_string()));
        }
        let trimmed = s.trim();
        let single = trimmed.starts_with("{{")
            && trimmed.ends_with("}}")
            && !trimmed[2..trimmed.len() - 2].contains("{{");
        if single {
            let expr = trimmed[2..trimmed.len() - 2].trim();
            let compiled = self
                .env
                .compile_expression(expr)
                .map_err(|e| CoreError::Template(format!("`{expr}`: {e}")))?;
            let result = compiled
                .eval(&self.ctx)
                .map_err(|e| CoreError::Template(format!("`{expr}`: {e}")))?;
            return serde_json::to_value(result)
                .map_err(|e| CoreError::Template(format!("`{expr}`: {e}")));
        }
        Ok(Value::String(self.render_str(s)?))
    }
}
