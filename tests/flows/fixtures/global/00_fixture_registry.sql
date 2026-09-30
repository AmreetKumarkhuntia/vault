-- Schema used only by the demo's suite-global fixture.
-- IF NOT EXISTS keeps it valid when reset mode truncates rather than drops tables.
CREATE TABLE IF NOT EXISTS vault_fixture_baseline (
    fixture_key TEXT PRIMARY KEY,
    fixture_value TEXT NOT NULL
);
