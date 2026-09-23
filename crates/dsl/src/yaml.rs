use serde_json::{json, Map, Value as Json};
use serde_yaml_ng::Value as Yaml;

/// Convert a YAML value to JSON, encoding YAML tags (`!uuid`, `!near-now 5s`)
/// as `{"$tag": "uuid"}` / `{"$tag": "near-now", "arg": "5s"}` matcher objects.
pub fn yaml_to_json(v: &Yaml) -> Json {
    match v {
        Yaml::Null => Json::Null,
        Yaml::Bool(b) => Json::Bool(*b),
        Yaml::Number(n) => {
            if let Some(i) = n.as_i64() {
                json!(i)
            } else if let Some(u) = n.as_u64() {
                json!(u)
            } else {
                json!(n.as_f64().unwrap_or(0.0))
            }
        }
        Yaml::String(s) => Json::String(s.clone()),
        Yaml::Sequence(items) => Json::Array(items.iter().map(yaml_to_json).collect()),
        Yaml::Mapping(m) => {
            let mut out = Map::new();
            for (k, val) in m {
                let key = match k {
                    Yaml::String(s) => s.clone(),
                    other => yaml_scalar_to_string(other),
                };
                out.insert(key, yaml_to_json(val));
            }
            Json::Object(out)
        }
        Yaml::Tagged(tagged) => {
            let tag = tagged.tag.to_string();
            let tag = tag.trim_start_matches('!').to_string();
            let arg = yaml_to_json(&tagged.value);
            let mut out = Map::new();
            out.insert("$tag".into(), Json::String(tag));
            if !arg.is_null() {
                out.insert("arg".into(), arg);
            }
            Json::Object(out)
        }
    }
}

fn yaml_scalar_to_string(v: &Yaml) -> String {
    match v {
        Yaml::String(s) => s.clone(),
        Yaml::Bool(b) => b.to_string(),
        Yaml::Number(n) => n.to_string(),
        Yaml::Null => "null".into(),
        other => format!("{other:?}"),
    }
}

/// Config-time interpolation: `${env.VAR}` and `${env.VAR:-default}`.
/// This is the ONLY substitution allowed in `vault.yaml`; the `{{ }}`
/// engine is test-time only. Innermost-first, so a default may itself
/// contain a reference: `${env.PG_URL:-postgres://${env.USER}@localhost/db}`.
pub fn interpolate_env(raw: &str) -> String {
    let mut s = raw.to_string();
    for _ in 0..8 {
        let Some(start) = s.rfind("${env.") else {
            break;
        };
        let Some(end_rel) = s[start + 6..].find('}') else {
            break;
        };
        let expr = &s[start + 6..start + 6 + end_rel];
        let (name, default) = match expr.split_once(":-") {
            Some((n, d)) => (n, Some(d.to_string())),
            None => (expr, None),
        };
        let value = std::env::var(name).unwrap_or_else(|_| default.unwrap_or_default());
        s.replace_range(start..start + 6 + end_rel + 1, &value);
    }
    s
}
