-- Multiple statements intentionally exercise raw SQL fixture execution.
INSERT INTO users (id, email, status, plan)
VALUES (7001, 'sql-fixture@example.com', 'active', 'pro');

INSERT INTO wallets (user_id, balance_cents, currency)
VALUES (7001, 42000, 'USD');

INSERT INTO orders (id, user_id, status, total_cents, external_charge_id)
VALUES (7101, 7001, 'seeded', 3200, 'fixture_charge');

INSERT INTO audit_log (entity, entity_id, action)
VALUES ('order', '7101', 'fixture-loaded');
