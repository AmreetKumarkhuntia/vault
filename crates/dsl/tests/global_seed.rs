use serde_json::json;
use vault_dsl::{parse_str, Config};

#[test]
fn config_accepts_defaulted_global_seed_documents() {
    let without_seed: Config = parse_str(
        "environments: { local: { target: { base_url: 'http://example.test' } } }",
        "vault.yaml",
    )
    .unwrap();
    assert!(without_seed.seed.is_empty());

    let with_seed: Config = parse_str(
        r#"
environments:
  local:
    target: { base_url: "http://example.test" }
seed:
  postgres:
    - sql_file: ../../shared/base.sql
    - sql_glob: /opt/fixtures/*.sql
  redis:
    - set: { key: global, value: enabled }
"#,
        "vault.yaml",
    )
    .unwrap();

    assert_eq!(
        with_seed.seed["postgres"],
        json!([
            {"sql_file": "../../shared/base.sql"},
            {"sql_glob": "/opt/fixtures/*.sql"}
        ])
    );
    assert_eq!(
        with_seed.seed["redis"],
        json!([{"set": {"key": "global", "value": "enabled"}}])
    );
}
