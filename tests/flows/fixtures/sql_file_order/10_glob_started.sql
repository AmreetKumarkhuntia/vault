-- The WHERE clause makes the glob's lexical execution order observable.
UPDATE orders
SET status = 'fixture-glob-started'
WHERE id = 7101 AND status = 'seeded';
