-- This file is selected by vault.yaml's fixtures/global/*.seed.sql glob.
INSERT INTO vault_fixture_baseline (fixture_key, fixture_value)
VALUES ('suite-global', 'loaded')
ON CONFLICT (fixture_key)
DO UPDATE SET fixture_value = EXCLUDED.fixture_value;
