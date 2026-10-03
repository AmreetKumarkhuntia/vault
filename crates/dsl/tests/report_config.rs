use vault_dsl::{parse_str, Config};

#[test]
fn report_html_and_extra_masking_rules_are_optional() {
    let base = "environments: { local: { target: { base_url: 'http://example.test' } } }";
    let config: Config = parse_str(base, "vault.yaml").unwrap();
    assert!(config.report.html.is_none());
    assert!(config.report.redact.headers.is_empty());

    let configured: Config = parse_str(
        &format!(
            "{base}\nreport:\n  html: target/report.html\n  redact:\n    headers: [x-company-auth]\n    fields: [pin]\n    json_paths: ['$.profile.identifier']\n    text_patterns: ['account-[0-9]+']\n"
        ),
        "vault.yaml",
    )
    .unwrap();
    assert_eq!(
        configured.report.html.as_deref(),
        Some("target/report.html")
    );
    assert_eq!(configured.report.redact.headers, ["x-company-auth"]);
    assert_eq!(configured.report.redact.fields, ["pin"]);
    assert_eq!(
        configured.report.redact.json_paths,
        ["$.profile.identifier"]
    );
    assert_eq!(configured.report.redact.text_patterns, ["account-[0-9]+"]);
}

#[test]
fn report_masking_configuration_rejects_unknown_fields() {
    let config = "environments: { local: { target: { base_url: 'http://example.test' } } }\nreport:\n  redact:\n    disable_defaults: true\n";
    assert!(parse_str::<Config>(config, "vault.yaml").is_err());
    assert!(parse_str::<Config>(&config.replace("redact:", "unknown:"), "vault.yaml").is_err());
}
