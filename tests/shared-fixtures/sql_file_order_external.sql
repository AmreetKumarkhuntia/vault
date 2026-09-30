-- Referenced from tests/flows/orders with ../../ to prove fixtures may live
-- outside both the declaring file's directory and the suite directory.
UPDATE orders
SET status = 'fixture-external'
WHERE id = 7101 AND status = 'fixture-glob-complete';
