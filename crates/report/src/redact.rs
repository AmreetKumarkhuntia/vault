//! Report-only masking. Discover all report copies before sanitizing them so a
//! credential found in one payload is also removed from derived diagnostics.

use std::collections::BTreeSet;

use regex::Regex;
use serde_json::Value;
use serde_json_path::JsonPath;
use vault_core::RunResult;
use vault_dsl::ReportRedaction;
use vault_store::{CheckResult, FailureKind, FieldDiff, VerifyOutcome};

const MASK: &str = "[REDACTED]";
const DEFAULT_FIELDS: &[&str] = &[
    "password",
    "passwd",
    "pwd",
    "secret",
    "client_secret",
    "token",
    "access_token",
    "refresh_token",
    "id_token",
    "api_key",
    "api_token",
    "apikey",
    "auth",
    "access_key",
    "secret_key",
    "secret_access_key",
    "signing_key",
    "password_hash",
    "passphrase",
    "private_key",
    "session_token",
    "session_id",
    "sessionid",
    "credentials",
    "authorization",
    "proxy-authorization",
    "cookie",
    "set-cookie",
    "x-api-key",
    "api-key",
    "x-auth-token",
    "x-access-token",
    "x-amz-security-token",
    "x-goog-api-key",
    "x-csrf-token",
    "x-xsrf-token",
    "authentication",
    "auth_token",
    "bearer_token",
    "csrf_token",
    "xsrf_token",
];

/// Stateful per-run redactor. Do not share discovered credentials between runs.
pub struct Redactor {
    fields: BTreeSet<String>,
    paths: Vec<(String, JsonPath)>,
    patterns: Vec<Regex>,
    secrets: BTreeSet<String>,
    protected_paths: BTreeSet<String>,
    urls: Regex,
    assignments: Regex,
    credential_lines: Regex,
}

impl Redactor {
    /// Compile configuration before execution, so invalid rules fail preflight.
    pub fn new(config: &ReportRedaction) -> Result<Self, String> {
        let fields: BTreeSet<String> = DEFAULT_FIELDS
            .iter()
            .map(|s| normalize(s))
            .chain(config.headers.iter().map(|s| normalize(s)))
            .chain(config.fields.iter().map(|s| normalize(s)))
            .collect();
        if fields.contains("") {
            return Err("report.redact headers and fields must not be empty".into());
        }
        let paths = config
            .json_paths
            .iter()
            .map(|source| {
                JsonPath::parse(source)
                    .map(|path| (source.clone(), path))
                    .map_err(|e| {
                        format!("invalid report.redact.json_paths selector `{source}`: {e}")
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let patterns = config
            .text_patterns
            .iter()
            .map(|source| {
                let pattern = Regex::new(source).map_err(|e| {
                    format!("invalid report.redact.text_patterns regex `{source}`: {e}")
                })?;
                if pattern.is_match("") {
                    return Err(format!(
                        "report.redact.text_patterns regex `{source}` matches empty text"
                    ));
                }
                Ok(pattern)
            })
            .collect::<Result<Vec<_>, String>>()?;
        // Support spelling separators in plain text as well as object keys.
        let names = fields
            .iter()
            .map(|name| {
                name.chars()
                    .map(|c| regex::escape(&c.to_string()))
                    .collect::<Vec<_>>()
                    .join("[-_ ]*")
            })
            .collect::<Vec<_>>()
            .join("|");
        let assignments = Regex::new(&format!(
            r#"(?i)(\b(?:{names})\b["']?\s*[:=]\s*)(?:"([^"]*)"|'([^']*)'|([^\s,;"'\}}\]&]+))"#
        ))
        .map_err(|e| format!("cannot compile report masking rules: {e}"))?;
        Ok(Self {
            fields,
            paths,
            patterns,
            secrets: BTreeSet::new(),
            protected_paths: BTreeSet::new(),
            urls: Regex::new(r#"[a-zA-Z][a-zA-Z0-9+.-]*://[^\s<>"']+"#)
                .expect("constant URL regex"),
            assignments,
            credential_lines: Regex::new(
                r"(?im)((?:proxy-)?authorization|(?:set-)?cookie)\s*:\s*([^\r\n]+)",
            )
            .expect("constant credential line regex"),
        })
    }

    /// Discover credentials without modifying matching inputs or exports.
    pub fn discover(&mut self, value: &Value) {
        self.collect_sensitive_locations(value);
        self.discover_inner(value);
    }

    fn collect_sensitive_locations(&mut self, value: &Value) {
        let locations: Vec<String> = self
            .paths
            .iter()
            .flat_map(|(_, path)| {
                path.query_located(value)
                    .all()
                    .into_iter()
                    .map(|node| normalize_path(&node.location().to_string()))
            })
            .collect();
        self.protected_paths.extend(locations);
        match value {
            Value::Object(object) => object
                .values()
                .for_each(|value| self.collect_sensitive_locations(value)),
            Value::Array(values) => values
                .iter()
                .for_each(|value| self.collect_sensitive_locations(value)),
            Value::String(text) => {
                if let Some(json) = parse_container(text) {
                    self.collect_sensitive_locations(&json);
                }
            }
            _ => {}
        }
    }

    fn discover_inner(&mut self, value: &Value) {
        let selected: Vec<Value> = self
            .paths
            .iter()
            .flat_map(|(_, path)| path.query(value).all().into_iter().cloned())
            .collect();
        for secret in &selected {
            self.remember_value(secret);
        }
        match value {
            Value::Object(object) => {
                let sensitive_diff = self.sensitive_location(object);
                if self.sensitive_capture(object) {
                    if let Some(value) = object
                        .get("details")
                        .and_then(|details| details.get("value"))
                    {
                        self.remember_value(value);
                    }
                }
                let sensitive_named_value = object
                    .get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|name| self.is_sensitive(name));
                for (key, child) in object {
                    if self.is_sensitive(key)
                        || (sensitive_diff && matches!(key.as_str(), "expected" | "actual" | "ops"))
                        || (sensitive_named_value && key == "value")
                    {
                        self.remember_value(child);
                        let field = if key == "value" {
                            object.get("name").and_then(Value::as_str).unwrap_or(key)
                        } else {
                            key
                        };
                        self.discover_header_parts(field, child);
                    }
                    self.discover_inner(child);
                }
            }
            Value::Array(values) => values.iter().for_each(|v| self.discover_inner(v)),
            Value::String(text) => self.discover_text(text),
            _ => {}
        }
    }

    /// Discover and sanitize a report JSON copy. Calling `discover` for all
    /// sibling report copies first also masks cross-payload diagnostic echoes.
    pub fn sanitize(&mut self, value: &mut Value) {
        self.discover(value);
        self.scrub(value);
    }

    /// Discover only execution payloads and user-authored subjects. Machine
    /// statuses and phase names never become credentials merely because a
    /// custom rule names a field such as `status`.
    pub fn discover_execution(&mut self, execution: &Value) {
        if let Some(exports) = execution.get("exports") {
            self.discover(exports);
        }
        if let Some(events) = execution.get("events").and_then(Value::as_array) {
            for event in events {
                if let Some(subject) = event.get("subject") {
                    self.discover_user_field("subject", subject);
                }
                if let Some(details) = event.get("details") {
                    if event
                        .as_object()
                        .is_some_and(|event| self.sensitive_capture(event))
                    {
                        if let Some(value) = details.get("value") {
                            self.remember_value(value);
                        }
                    }
                    self.discover(details);
                }
            }
        }
    }

    /// Mask companion execution evidence while preserving its ordinals,
    /// timings, action statuses and phases. Payloads have no such exemptions.
    pub fn sanitize_execution(&mut self, execution: &mut Value) {
        self.discover_execution(execution);
        if let Some(exports) = execution.get_mut("exports") {
            self.scrub(exports);
        }
        if let Some(events) = execution.get_mut("events").and_then(Value::as_array_mut) {
            for event in events {
                let sensitive_capture = event
                    .as_object()
                    .is_some_and(|event| self.sensitive_capture(event));
                if let Some(details) = event.get_mut("details") {
                    if sensitive_capture {
                        if let Some(value) = details.get_mut("value") {
                            *value = Value::String(MASK.into());
                        }
                    }
                    self.scrub(details);
                }
                if let Some(subject) = event.get_mut("subject") {
                    self.scrub_user_field("subject", subject);
                }
            }
        }
    }

    /// Discover metadata prose and stage inputs, excluding generated IDs,
    /// indices, policy enums, shuffle seeds and run timestamps.
    pub fn discover_metadata(&mut self, metadata: &Value) {
        for key in ["suite", "pattern", "tags"] {
            if let Some(value) = metadata.get(key) {
                self.discover_user_field(key, value);
            }
        }
        for collection in ["items", "tests"] {
            if let Some(items) = metadata.get(collection).and_then(Value::as_array) {
                for item in items {
                    for key in [
                        "name",
                        "description",
                        "tags",
                        "source",
                        "flow",
                        "export",
                        "step_names",
                    ] {
                        if let Some(value) = item.get(key) {
                            self.discover_user_field(key, value);
                        }
                    }
                    if let Some(value) = item.get("with") {
                        self.discover(value);
                    }
                }
            }
        }
    }

    /// Mask report navigation text without changing identity or array shapes.
    pub fn sanitize_metadata(&mut self, metadata: &mut Value) {
        self.discover_metadata(metadata);
        for key in ["suite", "pattern", "tags"] {
            if let Some(value) = metadata.get_mut(key) {
                self.scrub_user_field(key, value);
            }
        }
        for collection in ["items", "tests"] {
            if let Some(items) = metadata.get_mut(collection).and_then(Value::as_array_mut) {
                for item in items {
                    for key in [
                        "name",
                        "description",
                        "tags",
                        "source",
                        "flow",
                        "export",
                        "step_names",
                    ] {
                        if let Some(value) = item.get_mut(key) {
                            self.scrub_user_field(key, value);
                        }
                    }
                    if let Some(value) = item.get_mut("with") {
                        self.scrub(value);
                    }
                }
            }
        }
    }

    fn discover_user_field(&mut self, key: &str, value: &Value) {
        if self.is_sensitive(key) {
            self.remember_value(value);
        }
        self.discover(value);
    }

    fn scrub_user_field(&self, key: &str, value: &mut Value) {
        match value {
            Value::String(text) => {
                *text = if self.is_sensitive(key) {
                    MASK.into()
                } else {
                    self.scrub_text(text)
                };
            }
            Value::Array(values) => values
                .iter_mut()
                .for_each(|value| self.scrub_user_field(key, value)),
            Value::Null => {}
            value => self.scrub(value),
        }
    }

    /// Preserve the existing result types and serialized schema while masking
    /// every human-visible result field before terminal/JSON/JUnit rendering.
    pub fn sanitize_run(&mut self, run: &mut RunResult) {
        if let Ok(value) = serde_json::to_value(&*run) {
            self.discover(&value);
        }
        run.environment = self.scrub_text(&run.environment);
        for test in &mut run.tests {
            test.name = self.scrub_text(&test.name);
            for text in [&mut test.skip_reason, &mut test.error, &mut test.flow]
                .into_iter()
                .flatten()
            {
                *text = self.scrub_text(text);
            }
            for text in &mut test.seed_receipts {
                *text = self.scrub_text(text);
            }
            let mut captures = Value::Object(std::mem::take(&mut test.captures));
            self.scrub(&mut captures);
            if let Value::Object(object) = captures {
                test.captures = object;
            }
            for recording in &mut test.recorded_calls {
                self.scrub(recording);
            }
            for step in &mut test.steps {
                step.name = self.scrub_text(&step.name);
                if let Some(response) = &mut step.response {
                    self.scrub(&mut response.body);
                }
                self.scrub_checks(&mut step.checks);
            }
            self.scrub_checks(&mut test.verify);
        }
    }

    fn scrub_checks(&self, outcome: &mut VerifyOutcome) {
        for check in &mut outcome.checks {
            match check {
                CheckResult::Pass { description } => *description = self.scrub_text(description),
                CheckResult::Fail(failure) => {
                    failure.description = self.scrub_text(&failure.description);
                    self.scrub(&mut failure.expected);
                    if self.is_sensitive_path(&failure.yaml_path) {
                        failure.expected = Value::String(MASK.into());
                    }
                    failure.yaml_path = self.scrub_text(&failure.yaml_path);
                    match &mut failure.kind {
                        FailureKind::UnexpectedRow { actual } => self.scrub(actual),
                        FailureKind::UnexpectedChange { before, after } => {
                            self.scrub(before);
                            self.scrub(after);
                        }
                        FailureKind::ValueMismatch { diffs } => self.scrub_diffs(diffs),
                        FailureKind::UnexpectedKey { key } => *key = self.scrub_text(key),
                        FailureKind::CountMismatch { expected, .. } => {
                            *expected = self.scrub_text(expected)
                        }
                        FailureKind::UnexpectedCall { exchange } => self.scrub(exchange),
                        FailureKind::OrderViolation { interleaving } => {
                            for (name, _) in interleaving {
                                *name = self.scrub_text(name);
                            }
                        }
                        _ => {}
                    }
                    for near in &mut failure.near_misses {
                        self.scrub(&mut near.actual);
                        self.scrub_diffs(&mut near.diffs);
                    }
                }
            }
        }
    }

    fn scrub_diffs(&self, diffs: &mut [FieldDiff]) {
        for diff in diffs {
            if self.is_sensitive_path(&diff.path) {
                diff.expected = Value::String(MASK.into());
                diff.actual = Value::String(MASK.into());
            } else {
                self.scrub(&mut diff.expected);
                self.scrub(&mut diff.actual);
            }
            diff.path = self.scrub_text(&diff.path);
        }
    }

    fn scrub(&self, value: &mut Value) {
        let selected: Vec<String> = self
            .paths
            .iter()
            .flat_map(|(_, path)| {
                path.query_located(value)
                    .all()
                    .into_iter()
                    .map(|node| node.location().to_json_pointer())
            })
            .collect();
        for pointer in selected {
            if let Some(secret) = value.pointer_mut(&pointer) {
                *secret = Value::String(MASK.into());
            }
        }
        match value {
            Value::Object(object) => {
                let sensitive_diff = self.sensitive_location(object);
                if self.sensitive_capture(object) {
                    if let Some(value) = object
                        .get_mut("details")
                        .and_then(|details| details.get_mut("value"))
                    {
                        *value = Value::String(MASK.into());
                    }
                }
                let sensitive_named_value = object
                    .get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|name| self.is_sensitive(name));
                for (key, child) in object {
                    if self.is_sensitive(key)
                        || (sensitive_diff && matches!(key.as_str(), "expected" | "actual" | "ops"))
                        || (sensitive_named_value && key == "value")
                    {
                        *child = Value::String(MASK.into());
                    } else {
                        self.scrub(child);
                    }
                }
            }
            Value::Array(values) => values.iter_mut().for_each(|v| self.scrub(v)),
            Value::String(text) => *text = self.scrub_text(text),
            _ => {}
        }
    }

    fn sensitive_location(&self, object: &serde_json::Map<String, Value>) -> bool {
        ["path", "yaml_path"].iter().any(|key| {
            object
                .get(*key)
                .and_then(Value::as_str)
                .is_some_and(|p| self.is_sensitive_path(p))
        })
    }

    fn sensitive_capture(&self, object: &serde_json::Map<String, Value>) -> bool {
        object.get("phase").and_then(Value::as_str) == Some("capture")
            && object
                .get("subject")
                .and_then(Value::as_str)
                .is_some_and(|name| self.is_sensitive(name))
    }

    fn is_sensitive(&self, field: &str) -> bool {
        self.fields.contains(&normalize(field))
    }

    fn is_sensitive_path(&self, path: &str) -> bool {
        let normalized = normalize_path(path);
        path.split(['.', '/', '[', ']', '\'', '"'])
            .any(|part| self.is_sensitive(part))
            || self.paths.iter().any(|(selector, _)| selector == path)
            || self.protected_paths.iter().any(|protected| {
                protected.is_empty()
                    || normalized == *protected
                    || normalized
                        .strip_prefix(protected)
                        .is_some_and(|suffix| suffix.starts_with('/'))
            })
    }

    fn remember_value(&mut self, value: &Value) {
        match value {
            Value::String(text) => self.remember(text),
            Value::Array(values) => values.iter().for_each(|v| self.remember_value(v)),
            Value::Object(values) => values.values().for_each(|v| self.remember_value(v)),
            Value::Number(number) => self.remember(&number.to_string()),
            _ => {}
        }
    }

    fn discover_header_parts(&mut self, field: &str, value: &Value) {
        match value {
            Value::String(text) => match normalize(field).as_str() {
                "authorization" | "proxyauthorization" => {
                    if let Some((_, credential)) = text.split_once(' ') {
                        self.remember(credential);
                    }
                }
                "cookie" => {
                    for pair in text.split(';') {
                        if let Some((_, credential)) = pair.trim().split_once('=') {
                            self.remember(credential);
                        }
                    }
                }
                "setcookie" => {
                    if let Some((_, credential)) = text
                        .split(';')
                        .next()
                        .and_then(|pair| pair.trim().split_once('='))
                    {
                        self.remember(credential);
                    }
                }
                _ => {}
            },
            Value::Array(values) => values
                .iter()
                .for_each(|value| self.discover_header_parts(field, value)),
            _ => {}
        }
    }

    fn remember(&mut self, text: &str) {
        // A one-character password must be masked at its source, but replacing
        // that character throughout prose would destroy statuses and IDs.
        if text.chars().count() >= 4
            && !matches!(
                text.to_ascii_lowercase().as_str(),
                "true" | "false" | "null" | "none"
            )
            && text != MASK
        {
            self.secrets.insert(text.to_owned());
            if let Ok(encoded) = serde_json::to_string(text) {
                self.secrets
                    .insert(encoded[1..encoded.len() - 1].to_owned());
            }
            // Encoded credentials can also be echoed outside a URL or form,
            // for example in a test name or a multiline transport diagnostic.
            let encoded: String = url::form_urlencoded::byte_serialize(text.as_bytes()).collect();
            self.secrets.insert(encoded.replace('+', "%20"));
            self.secrets.insert(encoded);
        }
    }

    fn discover_text(&mut self, text: &str) {
        if let Some(json) = parse_container(text) {
            self.discover(&json);
        }
        let assignments: Vec<String> = self
            .assignments
            .captures_iter(text)
            .filter_map(|captures| {
                (2..=4).find_map(|index| captures.get(index).map(|v| v.as_str().to_owned()))
            })
            .collect();
        for secret in assignments {
            self.remember(&secret);
        }
        let form_secrets: Vec<String> = url::form_urlencoded::parse(text.as_bytes())
            .filter(|(name, _)| self.is_sensitive(name))
            .map(|(_, value)| value.into_owned())
            .collect();
        for secret in form_secrets {
            self.remember(&secret);
        }
        let lines: Vec<(String, String)> = self
            .credential_lines
            .captures_iter(text)
            .map(|captures| (captures[1].to_owned(), captures[2].to_owned()))
            .collect();
        for (field, secret) in lines {
            self.remember(&secret);
            self.discover_header_parts(&field, &Value::String(secret));
        }
        let pattern_matches: Vec<String> = self
            .patterns
            .iter()
            .flat_map(|regex| regex.find_iter(text).map(|m| m.as_str().to_owned()))
            .collect();
        for secret in pattern_matches {
            self.remember(&secret);
        }
        let urls: Vec<String> = self
            .urls
            .find_iter(text)
            .map(|m| m.as_str().to_owned())
            .collect();
        for raw in urls {
            if let Ok(url) = url::Url::parse(&raw) {
                if !url.username().is_empty() {
                    self.remember(url.username());
                    let encoded = format!("v={}", url.username());
                    if let Some((_, decoded)) =
                        url::form_urlencoded::parse(encoded.as_bytes()).next()
                    {
                        self.remember(&decoded);
                    }
                }
                if let Some(password) = url.password() {
                    self.remember(password);
                    // Query decoding also handles percent escapes in userinfo.
                    let encoded = format!("v={password}");
                    if let Some((_, decoded)) =
                        url::form_urlencoded::parse(encoded.as_bytes()).next()
                    {
                        self.remember(&decoded);
                    }
                }
                for (name, value) in url.query_pairs() {
                    if self.is_sensitive(&name) {
                        self.remember(&value);
                    }
                }
            }
        }
    }

    fn scrub_text(&self, text: &str) -> String {
        if text == MASK {
            return text.to_owned();
        }
        let mut output = if let Some(masked) = self.mask_truncated_credential(text) {
            masked
        } else if let Some(mut json) = parse_container(text) {
            self.scrub(&mut json);
            if serde_json::from_str::<Value>(text).ok().as_ref() == Some(&json) {
                text.to_owned()
            } else {
                json.to_string()
            }
        } else {
            text.to_owned()
        };
        let form_pairs: Vec<_> = url::form_urlencoded::parse(output.as_bytes())
            .map(|(name, value)| (name.into_owned(), value.into_owned()))
            .collect();
        // A whole URL or prose containing '=' must not be rewritten as a form.
        let is_form = output.contains('=')
            && form_pairs.iter().all(|(name, _)| {
                !name.is_empty()
                    && name
                        .chars()
                        .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '[' | ']'))
            });
        if is_form {
            let mut serializer = url::form_urlencoded::Serializer::new(String::new());
            let mut changed = false;
            for (name, value) in &form_pairs {
                let safe = if self.is_sensitive(name) {
                    MASK.into()
                } else {
                    self.mask_known_text(value)
                };
                changed |= safe != *value;
                serializer.append_pair(name, &safe);
            }
            if changed {
                output = serializer.finish();
            }
        }
        output = self
            .urls
            .replace_all(&output, |captures: &regex::Captures<'_>| {
                let raw = captures.get(0).map_or("", |v| v.as_str());
                let Ok(mut url) = url::Url::parse(raw) else {
                    return raw.to_owned();
                };
                let mut changed = false;
                if !url.username().is_empty() {
                    let _ = url.set_username(MASK);
                    changed = true;
                }
                if url.password().is_some() {
                    let _ = url.set_password(Some(MASK));
                    changed = true;
                }
                let pairs: Vec<(String, String)> = url
                    .query_pairs()
                    .map(|(name, value)| {
                        if self.is_sensitive(&name) {
                            changed = true;
                            (name.into_owned(), MASK.into())
                        } else {
                            let safe = self.mask_known_text(&value);
                            changed |= safe != value;
                            (name.into_owned(), safe)
                        }
                    })
                    .collect();
                if changed {
                    if url.query().is_some() {
                        url.query_pairs_mut().clear().extend_pairs(pairs);
                    }
                    url.to_string()
                } else {
                    raw.to_owned()
                }
            })
            .into_owned();
        output = self
            .assignments
            .replace_all(&output, |captures: &regex::Captures<'_>| {
                format!("{}{MASK}", &captures[1])
            })
            .into_owned();
        output = self
            .credential_lines
            .replace_all(&output, |captures: &regex::Captures<'_>| {
                format!("{}: {MASK}", &captures[1])
            })
            .into_owned();
        self.mask_known_text(&output)
    }

    fn mask_known_text(&self, text: &str) -> String {
        let mut output = text.to_owned();
        // Longest first prevents partial masking from exposing token suffixes.
        let mut secrets: Vec<&str> = self.secrets.iter().map(String::as_str).collect();
        secrets.sort_unstable_by_key(|secret| std::cmp::Reverse(secret.len()));
        for secret in secrets {
            output = output.replace(secret, MASK);
        }
        for pattern in &self.patterns {
            output = pattern.replace_all(&output, MASK).into_owned();
        }
        output
    }

    fn mask_truncated_credential(&self, text: &str) -> Option<String> {
        let prefix = text.strip_suffix('…')?;
        // These are the pre-existing raw-body previews in the core runner and
        // HTTP mismatch diagnostics. HTML preview limits run after redaction.
        if ![500usize, 2000]
            .iter()
            .any(|limit| prefix.len() <= *limit && prefix.len() + 3 >= *limit)
        {
            return None;
        }
        let mut first_secret = None;
        for secret in &self.secrets {
            let first = secret.chars().next()?;
            for (start, _) in prefix.match_indices(first) {
                if first_secret.is_some_and(|earliest| start >= earliest) {
                    break;
                }
                let fragment = &prefix[start..];
                // Only a substantial, strictly incomplete credential qualifies;
                // short prefixes and ordinary prose ending in ellipsis stay intact.
                if fragment.len() < secret.len()
                    && fragment.chars().count() >= 16
                    && secret.starts_with(fragment)
                {
                    first_secret = Some(start);
                    break;
                }
            }
        }
        first_secret.map(|start| format!("{}{MASK}…", &prefix[..start]))
    }
}

fn normalize(text: &str) -> String {
    text.chars()
        .filter(|c| !matches!(c, '-' | '_' | ' '))
        .flat_map(char::to_lowercase)
        .collect()
}

fn normalize_path(path: &str) -> String {
    path.split(['.', '/', '[', ']', '\'', '"'])
        .filter(|part| !part.is_empty() && *part != "$")
        .collect::<Vec<_>>()
        .join("/")
}

fn parse_container(text: &str) -> Option<Value> {
    let trimmed = text.trim_start();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        serde_json::from_str(text).ok()
    } else {
        None
    }
}
