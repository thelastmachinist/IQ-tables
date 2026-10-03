//! SQL engine, structure changes and table rules.

use crate::app::App;
use crate::json::Json;
use crate::sql_exec::Out;

fn db() -> App {
    crate::host::STORE.with(|s| s.borrow_mut().clear());
    let mut app = App::new();
    app.drafts.push(crate::state::Draft::new("k".into(), "shop".into()));
    app
}

fn run(app: &mut App, q: &str) -> Vec<Out> {
    app.run_sql("k", q)
}

/// Run, expecting success; returns the messages.
fn ok(app: &mut App, q: &str) -> String {
    let out = run(app, q);
    let mut msgs = vec![];
    for o in &out {
        match o {
            Out::Msg(false, m) => panic!("{} → {}", q, m),
            Out::Msg(true, m) => msgs.push(m.clone()),
            Out::Rows { .. } => {}
        }
    }
    msgs.join(" | ")
}

fn err(app: &mut App, q: &str) -> String {
    let out = run(app, q);
    out.iter().find_map(|o| if let Out::Msg(false, m) = o { Some(m.clone()) } else { None }).unwrap_or_else(|| panic!("{} should fail, got {:?}", q, out))
}

/// Rows of the last result set, as text.
fn rows(app: &mut App, q: &str) -> Vec<Vec<String>> {
    let out = run(app, q);
    for o in out.iter().rev() {
        match o {
            Out::Rows { rows, .. } => {
                return rows.iter().map(|r| r.iter().map(|v| if v.is_null() { "NULL".to_string() } else { v.cell_text() }).collect()).collect()
            }
            Out::Msg(false, m) => panic!("{} → {}", q, m),
            _ => {}
        }
    }
    panic!("{} returned no rows: {:?}", q, out)
}

fn col(app: &mut App, q: &str) -> Vec<String> {
    rows(app, q).into_iter().map(|mut r| r.remove(0)).collect()
}

fn one(app: &mut App, q: &str) -> String {
    rows(app, q)[0][0].clone()
}

fn shop() -> App {
    let mut a = db();
    ok(
        &mut a,
        "CREATE TABLE suppliers (
            id INT AUTO_INCREMENT PRIMARY KEY,
            name VARCHAR(100) NOT NULL UNIQUE,
            city VARCHAR(50),
            rating DECIMAL(3,1) DEFAULT 3.0
         );
         CREATE TABLE parts (
            sku VARCHAR(20) PRIMARY KEY,
            name VARCHAR(100) NOT NULL,
            qty INT NOT NULL DEFAULT 0,
            price DECIMAL(10,2),
            supplier_id INT,
            added DATE,
            kind ENUM('bolt','nut','washer') DEFAULT 'bolt',
            CONSTRAINT fk_sup FOREIGN KEY (supplier_id) REFERENCES suppliers (id) ON DELETE SET NULL,
            CHECK (qty >= 0)
         );
         INSERT INTO suppliers (name, city) VALUES ('Brazos Bolt', 'Waco'), ('Lone Star', 'Austin'), ('Gulf Coast', 'Houston');
         INSERT INTO parts VALUES
            ('B-1', 'Hex bolt', 120, 0.25, 1, '2026-01-05', 'bolt'),
            ('B-2', 'Carriage bolt', 40, 0.40, 1, '2026-02-10', 'bolt'),
            ('N-1', 'Lock nut', 300, 0.08, 2, '2026-01-20', 'nut'),
            ('W-1', 'Flat washer', 0, 0.02, NULL, '2026-03-01', 'washer'),
            ('N-2', 'Wing nut', 15, 0.30, 2, '2026-03-15', 'nut')",
    );
    a
}

#[test]
fn create_insert_and_types() {
    let mut a = shop();
    assert_eq!(col(&mut a, "SELECT id FROM suppliers ORDER BY id"), vec!["1", "2", "3"], "AUTO_INCREMENT numbers rows");
    assert_eq!(one(&mut a, "SELECT rating FROM suppliers WHERE id = 2"), "3.0", "DEFAULT applies");
    assert_eq!(one(&mut a, "SELECT price FROM parts WHERE sku = 'W-1'"), "0.02");
    // coercion on the way in
    ok(&mut a, "INSERT INTO parts (sku, name, qty, price, added) VALUES ('X-1', 'Test', '1,200', '$3.456', '9/28/2026')");
    assert_eq!(rows(&mut a, "SELECT qty, price, added, kind FROM parts WHERE sku = 'X-1'")[0], vec!["1200", "3.46", "2026-09-28", "bolt"]);
    assert!(err(&mut a, "INSERT INTO parts (sku, name, qty) VALUES ('X-2', 'Bad', 'lots')").contains("isn't a whole number"));
    assert!(err(&mut a, "INSERT INTO parts (sku, name, kind) VALUES ('X-3', 'Bad', 'screw')").contains("isn't one of the choices"));
    assert!(err(&mut a, "INSERT INTO parts (sku, qty) VALUES ('X-4', 1)").contains("can't be empty"));
    assert!(err(&mut a, "INSERT INTO parts (sku, name, qty) VALUES ('X-5', 'Neg', -1)").contains("breaks the rule"));
    assert!(err(&mut a, "INSERT INTO parts (sku, name, supplier_id) VALUES ('X-6', 'Orphan', 99)").contains("isn't in suppliers"));
    assert!(err(&mut a, "INSERT INTO suppliers (name) VALUES ('lone star')").contains("must be unique"), "UNIQUE is case-insensitive like MySQL");
    assert!(err(&mut a, "INSERT INTO parts (sku, name) VALUES ('B-1', 'Dup')").contains("already exists"));
    assert!(err(&mut a, "INSERT INTO parts (sku, name, added) VALUES ('X-7', 'Date', '2026-02-30')").contains("isn't a date"));
    // IGNORE / ON DUPLICATE KEY UPDATE / REPLACE
    ok(&mut a, "INSERT IGNORE INTO parts (sku, name) VALUES ('B-1', 'Dup'), ('X-8', 'New')");
    assert_eq!(one(&mut a, "SELECT name FROM parts WHERE sku = 'B-1'"), "Hex bolt");
    ok(&mut a, "INSERT INTO parts (sku, name, qty) VALUES ('B-1', 'x', 5) ON DUPLICATE KEY UPDATE qty = qty + VALUES(qty)");
    assert_eq!(one(&mut a, "SELECT qty FROM parts WHERE sku = 'B-1'"), "125");
    ok(&mut a, "REPLACE INTO parts (sku, name, qty) VALUES ('X-8', 'Replaced', 9)");
    assert_eq!(rows(&mut a, "SELECT name, qty, kind FROM parts WHERE sku = 'X-8'")[0], vec!["Replaced", "9", "bolt"]);
    // INSERT … SELECT, LAST_INSERT_ID
    ok(&mut a, "INSERT INTO suppliers (name, city) SELECT CONCAT(name, ' II'), city FROM suppliers WHERE city = 'Waco'");
    assert_eq!(one(&mut a, "SELECT LAST_INSERT_ID()"), "4");
    assert_eq!(one(&mut a, "SELECT name FROM suppliers WHERE id = LAST_INSERT_ID()"), "Brazos Bolt II");
}

#[test]
fn select_joins_groups_and_subqueries() {
    let mut a = shop();
    let r = rows(&mut a, "SELECT p.sku, s.name AS supplier FROM parts p JOIN suppliers s ON s.id = p.supplier_id ORDER BY p.sku");
    assert_eq!(r.len(), 4);
    assert_eq!(r[0], vec!["B-1", "Brazos Bolt"]);
    let r = rows(&mut a, "SELECT p.sku, s.name FROM parts p LEFT JOIN suppliers s ON s.id = p.supplier_id WHERE s.id IS NULL");
    assert_eq!(r, vec![vec!["W-1", "NULL"]]);
    let r =
        rows(&mut a, "SELECT s.name, COUNT(p.sku) AS n FROM parts p RIGHT JOIN suppliers s ON s.id = p.supplier_id GROUP BY s.name ORDER BY n DESC, s.name");
    assert_eq!(r, vec![vec!["Brazos Bolt", "2"], vec!["Lone Star", "2"], vec!["Gulf Coast", "0"]]);
    // GROUP BY / HAVING / aggregates
    let r = rows(
        &mut a,
        "SELECT kind, COUNT(*) n, SUM(qty) total, ROUND(AVG(price), 3) avgp, MIN(added), MAX(name) FROM parts GROUP BY kind HAVING total > 50 ORDER BY 3 DESC",
    );
    assert_eq!(r, vec![vec!["nut", "2", "315", "0.19", "2026-01-20", "Wing nut"], vec!["bolt", "2", "160", "0.325", "2026-01-05", "Hex bolt"]]);
    assert_eq!(one(&mut a, "SELECT GROUP_CONCAT(sku ORDER BY sku DESC SEPARATOR '/') FROM parts WHERE kind = 'nut'"), "N-2/N-1");
    assert_eq!(one(&mut a, "SELECT COUNT(DISTINCT kind) FROM parts"), "3");
    // subqueries: scalar, IN, correlated EXISTS, derived table
    assert_eq!(col(&mut a, "SELECT sku FROM parts WHERE price > (SELECT AVG(price) FROM parts) ORDER BY sku"), vec!["B-1", "B-2", "N-2"]);
    assert_eq!(col(&mut a, "SELECT name FROM suppliers WHERE id IN (SELECT supplier_id FROM parts WHERE kind = 'nut')"), vec!["Lone Star"]);
    assert_eq!(col(&mut a, "SELECT name FROM suppliers s WHERE NOT EXISTS (SELECT 1 FROM parts p WHERE p.supplier_id = s.id)"), vec!["Gulf Coast"]);
    assert_eq!(one(&mut a, "SELECT MAX(t.n) FROM (SELECT supplier_id, COUNT(*) AS n FROM parts GROUP BY supplier_id) AS t"), "2");
    assert_eq!(col(&mut a, "SELECT sku FROM parts WHERE qty > ALL (SELECT qty FROM parts WHERE kind = 'bolt')"), vec!["N-1"]);
    // set operations, CTEs
    assert_eq!(col(&mut a, "SELECT city FROM suppliers UNION SELECT 'Waco' ORDER BY 1"), vec!["Austin", "Houston", "Waco"]);
    assert_eq!(col(&mut a, "SELECT city FROM suppliers UNION ALL SELECT 'Waco'").len(), 4);
    assert_eq!(col(&mut a, "SELECT supplier_id FROM parts INTERSECT SELECT id FROM suppliers ORDER BY 1"), vec!["1", "2"]);
    assert_eq!(col(&mut a, "SELECT id FROM suppliers EXCEPT SELECT supplier_id FROM parts"), vec!["3"]);
    assert_eq!(one(&mut a, "WITH cheap AS (SELECT * FROM parts WHERE price < 0.3) SELECT COUNT(*) FROM cheap"), "3");
    assert_eq!(col(&mut a, "WITH RECURSIVE n (x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n WHERE x < 5) SELECT x FROM n"), vec!["1", "2", "3", "4", "5"]);
    // window functions
    let r = rows(&mut a, "SELECT sku, ROW_NUMBER() OVER (PARTITION BY kind ORDER BY qty DESC) rn, RANK() OVER (ORDER BY kind) rk, SUM(qty) OVER (ORDER BY sku) running, LAG(sku) OVER (ORDER BY sku) prev FROM parts ORDER BY sku");
    assert_eq!(r[0], vec!["B-1", "1", "1", "120", "NULL"]);
    assert_eq!(r[1], vec!["B-2", "2", "1", "160", "B-1"]);
    assert_eq!(r[2], vec!["N-1", "1", "3", "460", "B-2"]);
    assert_eq!(r[4], vec!["W-1", "1", "5", "475", "N-2"]);
    // ROLLUP
    let r = rows(&mut a, "SELECT kind, SUM(qty) FROM parts GROUP BY kind WITH ROLLUP");
    assert_eq!(r.last().unwrap(), &vec!["NULL".to_string(), "475".to_string()]);
    // DISTINCT, LIMIT/OFFSET, CASE, IN list, BETWEEN, LIKE, REGEXP
    assert_eq!(col(&mut a, "SELECT DISTINCT kind FROM parts ORDER BY kind LIMIT 1, 2"), vec!["nut", "washer"]);
    assert_eq!(
        col(&mut a, "SELECT CASE WHEN qty = 0 THEN 'out' WHEN qty < 50 THEN 'low' ELSE 'ok' END FROM parts ORDER BY sku"),
        vec!["ok", "low", "ok", "low", "out"]
    );
    assert_eq!(col(&mut a, "SELECT sku FROM parts WHERE name REGEXP '^(hex|lock) ' ORDER BY sku"), vec!["B-1", "N-1"]);
    assert_eq!(col(&mut a, "SELECT sku FROM parts WHERE added BETWEEN '2026-02-01' AND '2026-03-10' ORDER BY sku"), vec!["B-2", "W-1"]);
    assert!(err(&mut a, "SELECT name FROM parts p JOIN suppliers s ON s.id = p.supplier_id").contains("ambiguous"));
    assert!(err(&mut a, "SELECT nope FROM parts").contains("Unknown column"));
    assert!(err(&mut a, "SELECT sku FROM parts WHERE SUM(qty) > 1").contains("HAVING"));
    assert!(err(&mut a, "SELECT (SELECT sku FROM parts)").contains("more than one row"));
}

#[test]
fn functions() {
    let mut a = db();
    let f = |a: &mut App, e: &str| one(a, &format!("SELECT {}", e));
    assert_eq!(f(&mut a, "CONCAT_WS('-', 'a', NULL, 'b')"), "a-b");
    assert_eq!(f(&mut a, "SUBSTRING('fasteners', 2, 4)"), "aste");
    assert_eq!(f(&mut a, "SUBSTRING('fasteners' FROM -3)"), "ers");
    assert_eq!(f(&mut a, "TRIM(LEADING 'x' FROM 'xxabc')"), "abc");
    assert_eq!(f(&mut a, "LPAD('7', 3, '0')"), "007");
    assert_eq!(f(&mut a, "REPLACE('a-b-c', '-', '+')"), "a+b+c");
    assert_eq!(f(&mut a, "LOCATE('b', 'abcb', 3)"), "4");
    assert_eq!(f(&mut a, "SUBSTRING_INDEX('a.b.c', '.', -2)"), "b.c");
    assert_eq!(f(&mut a, "FORMAT(1234567.891, 2)"), "1,234,567.89");
    assert_eq!(f(&mut a, "ROUND(2.345, 2)"), "2.35");
    assert_eq!(f(&mut a, "ROUND(-2.5)"), "-3");
    assert_eq!(f(&mut a, "TRUNCATE(2.349, 2)"), "2.34");
    assert_eq!(f(&mut a, "0.1 + 0.2"), "0.3");
    assert_eq!(f(&mut a, "10 DIV 3"), "3");
    assert_eq!(f(&mut a, "MOD(10, 3)"), "1");
    assert_eq!(f(&mut a, "POWER(2, 10)"), "1024");
    assert_eq!(f(&mut a, "GREATEST(3, 9, 4)"), "9");
    assert_eq!(f(&mut a, "COALESCE(NULL, NULL, 'x')"), "x");
    assert_eq!(f(&mut a, "IF(1 > 2, 'a', 'b')"), "b");
    assert_eq!(f(&mut a, "NULLIF(5, 5)"), "NULL");
    assert_eq!(f(&mut a, "DATE_ADD('2026-01-31', INTERVAL 1 MONTH)"), "2026-02-28");
    assert_eq!(f(&mut a, "'2026-09-28' + INTERVAL 3 DAY"), "2026-10-01");
    assert_eq!(f(&mut a, "DATEDIFF('2026-10-01', '2026-09-28')"), "3");
    assert_eq!(f(&mut a, "DATE_FORMAT('2026-09-28 14:05:00', '%W, %M %D %Y %h:%i %p')"), "Monday, September 28th 2026 02:05 PM");
    assert_eq!(f(&mut a, "STR_TO_DATE('28/09/2026', '%d/%m/%Y')"), "2026-09-28");
    assert_eq!(f(&mut a, "TIMESTAMPDIFF(MONTH, '2026-01-15', '2026-03-14')"), "1");
    assert_eq!(f(&mut a, "DAYNAME('2026-09-28')"), "Monday");
    assert_eq!(f(&mut a, "WEEKDAY('2026-09-28')"), "0");
    assert_eq!(f(&mut a, "LAST_DAY('2028-02-10')"), "2028-02-29");
    assert_eq!(f(&mut a, "EXTRACT(YEAR FROM '2026-09-28')"), "2026");
    assert_eq!(f(&mut a, "CAST('12.7' AS SIGNED)"), "13");
    assert_eq!(f(&mut a, "CAST(3 AS DECIMAL(5,2))"), "3.00");
    assert_eq!(f(&mut a, "JSON_EXTRACT('{\"a\": {\"b\": [10, 20]}}', '$.a.b[1]')"), "20");
    assert_eq!(f(&mut a, "JSON_UNQUOTE(JSON_EXTRACT('{\"n\": \"hi\"}', '$.n'))"), "hi");
    assert_eq!(f(&mut a, "REGEXP_REPLACE('a1b22c333', '[0-9]+', '#')"), "a#b#c#");
    assert_eq!(f(&mut a, "REGEXP_SUBSTR('order 42 of 7', '[0-9]+', 1, 2)"), "7");
    assert_eq!(f(&mut a, "'abc' LIKE 'a\\\\_c'"), "0");
    assert_eq!(f(&mut a, "'a_c' LIKE 'a\\\\_c'"), "1");
    assert_eq!(f(&mut a, "1 <=> NULL"), "0");
    assert_eq!(f(&mut a, "NULL <=> NULL"), "1");
    assert_eq!(f(&mut a, "(1, 2) IN ((1, 2), (3, 4))"), "1");
    assert_eq!(f(&mut a, "UPPER(SHA2('abc', 256)) LIKE 'BA7816BF%'"), "1");
    ok(&mut a, "SET @x = 5");
    assert_eq!(f(&mut a, "@x * 2"), "10");
    assert_eq!(rows(&mut a, "VALUES ROW(1, 'a'), ROW(2, 'b')").len(), 2);
}

#[test]
fn update_delete_and_foreign_keys() {
    let mut a = shop();
    // UPDATE sees its own earlier assignments; ORDER BY … LIMIT
    ok(&mut a, "UPDATE parts SET qty = qty + 10, price = qty / 100 WHERE sku = 'W-1'");
    assert_eq!(rows(&mut a, "SELECT qty, price FROM parts WHERE sku = 'W-1'")[0], vec!["10", "0.10"]);
    ok(&mut a, "UPDATE parts SET qty = 1 ORDER BY qty DESC LIMIT 1");
    assert_eq!(one(&mut a, "SELECT qty FROM parts WHERE sku = 'N-1'"), "1");
    // multi-table UPDATE
    ok(&mut a, "UPDATE parts p JOIN suppliers s ON s.id = p.supplier_id SET p.name = CONCAT(p.name, ' (', s.city, ')') WHERE s.city = 'Austin'");
    assert_eq!(one(&mut a, "SELECT name FROM parts WHERE sku = 'N-2'"), "Wing nut (Austin)");
    // rules hold on UPDATE too
    assert!(err(&mut a, "UPDATE parts SET qty = -5 WHERE sku = 'B-1'").contains("breaks the rule"));
    assert!(err(&mut a, "UPDATE parts SET supplier_id = 42 WHERE sku = 'B-1'").contains("isn't in suppliers"));
    assert!(err(&mut a, "UPDATE suppliers SET name = 'Lone Star' WHERE id = 1").contains("must be unique"));
    // changing a primary key value
    ok(&mut a, "UPDATE parts SET sku = 'B-100' WHERE sku = 'B-1'");
    assert_eq!(one(&mut a, "SELECT COUNT(*) FROM parts WHERE sku IN ('B-1', 'B-100')"), "1");
    // ON DELETE SET NULL
    ok(&mut a, "DELETE FROM suppliers WHERE id = 2");
    assert_eq!(col(&mut a, "SELECT sku FROM parts WHERE supplier_id IS NULL ORDER BY sku"), vec!["N-1", "N-2", "W-1"]);
    // DELETE with a subquery
    ok(&mut a, "DELETE FROM parts WHERE supplier_id IN (SELECT id FROM suppliers WHERE city = 'Waco')");
    assert_eq!(one(&mut a, "SELECT COUNT(*) FROM parts"), "3");
    // RESTRICT (the default) and CASCADE
    ok(&mut a, "CREATE TABLE orders (id INT AUTO_INCREMENT PRIMARY KEY, sku VARCHAR(20) REFERENCES parts (sku)); INSERT INTO orders (sku) VALUES ('N-1')");
    assert!(err(&mut a, "DELETE FROM parts WHERE sku = 'N-1'").contains("still points to it"));
    ok(&mut a, "ALTER TABLE orders DROP FOREIGN KEY orders_ibfk_1, ADD CONSTRAINT o_part FOREIGN KEY (sku) REFERENCES parts (sku) ON DELETE CASCADE");
    let m = ok(&mut a, "DELETE FROM parts WHERE sku = 'N-1'");
    assert!(m.contains("orders deleted too"), "{}", m);
    assert_eq!(one(&mut a, "SELECT COUNT(*) FROM orders"), "0");
    // FOREIGN_KEY_CHECKS = 0 lets a dump load in any order
    ok(&mut a, "SET FOREIGN_KEY_CHECKS = 0; INSERT INTO orders (sku) VALUES ('ZZZ'); SET FOREIGN_KEY_CHECKS = 1");
    assert!(err(&mut a, "INSERT INTO orders (sku) VALUES ('YYY')").contains("isn't in parts"));
}

#[test]
fn alter_table() {
    let mut a = shop();
    ok(&mut a, "ALTER TABLE parts ADD COLUMN bin VARCHAR(10) NOT NULL DEFAULT 'A1' AFTER name, ADD notes TEXT");
    assert_eq!(rows(&mut a, "SELECT * FROM parts WHERE sku = 'B-1'")[0][..4].to_vec(), vec!["B-1", "Hex bolt", "A1", "120"]);
    ok(&mut a, "ALTER TABLE parts CHANGE name title VARCHAR(120) NOT NULL, RENAME COLUMN bin TO shelf");
    assert_eq!(one(&mut a, "SELECT title FROM parts WHERE sku = 'B-2'"), "Carriage bolt");
    assert!(err(&mut a, "SELECT name FROM parts").contains("Unknown column"));
    // types convert, or the change is refused
    ok(&mut a, "ALTER TABLE parts MODIFY qty DECIMAL(8,1) NOT NULL");
    assert_eq!(one(&mut a, "SELECT qty FROM parts WHERE sku = 'B-1'"), "120.0");
    assert!(err(&mut a, "ALTER TABLE parts MODIFY title INT").contains("Can't change"));
    assert!(err(&mut a, "ALTER TABLE parts MODIFY price DECIMAL(10,2) NOT NULL; ALTER TABLE parts MODIFY supplier_id INT NOT NULL").contains("NOT NULL"));
    // keys
    assert!(err(&mut a, "ALTER TABLE parts ADD UNIQUE (kind)").contains("more than once"));
    ok(&mut a, "ALTER TABLE parts ADD UNIQUE KEY uq_title (title), ADD INDEX by_kind (kind)");
    let idx = rows(&mut a, "SHOW INDEX FROM parts");
    assert!(idx.iter().any(|r| r[2] == "uq_title" && r[1] == "0"));
    ok(&mut a, "ALTER TABLE parts DROP INDEX by_kind");
    // primary key change
    assert!(err(&mut a, "ALTER TABLE parts ADD PRIMARY KEY (title)").contains("already has a primary key"));
    ok(&mut a, "ALTER TABLE parts DROP PRIMARY KEY, ADD PRIMARY KEY (title)");
    assert!(rows(&mut a, "SHOW COLUMNS FROM parts").iter().any(|r| r[0] == "title" && r[3] == "PRI"));
    // drop column, rename table, comment, auto increment
    ok(&mut a, "ALTER TABLE parts DROP COLUMN notes, COMMENT = 'Fasteners we stock', RENAME TO stock");
    assert!(err(&mut a, "SELECT * FROM parts").contains("No table"));
    assert_eq!(one(&mut a, "SELECT COUNT(*) FROM stock"), "5");
    assert!(!one(&mut a, "SHOW CREATE TABLE stock").is_empty());
    let ddl = rows(&mut a, "SHOW CREATE TABLE stock")[0][1].clone();
    assert!(
        ddl.contains("PRIMARY KEY (`title`)") && ddl.contains("COMMENT='Fasteners we stock'") && ddl.contains("CONSTRAINT `fk_sup` FOREIGN KEY"),
        "{}",
        ddl
    );
    ok(&mut a, "ALTER TABLE suppliers AUTO_INCREMENT = 100; INSERT INTO suppliers (name) VALUES ('Permian')");
    assert_eq!(one(&mut a, "SELECT id FROM suppliers WHERE name = 'Permian'"), "100");
    // CHECK constraints follow renames and are verified when added
    assert!(err(&mut a, "ALTER TABLE stock ADD CONSTRAINT cheap CHECK (price < 0.3)").contains("doesn't meet it"));
    ok(&mut a, "ALTER TABLE stock RENAME COLUMN qty TO on_hand");
    let ddl = rows(&mut a, "SHOW CREATE TABLE stock")[0][1].clone();
    assert!(ddl.contains("CHECK (`on_hand` >= 0)"), "{}", ddl);
    assert!(err(&mut a, "ALTER TABLE stock DROP COLUMN on_hand").contains("uses `on_hand`"));
    // no primary key given → automatic id; a later ADD PRIMARY KEY replaces it
    let m = ok(&mut a, "CREATE TABLE tags (tag VARCHAR(20), label VARCHAR(50))");
    assert!(m.contains("automatic `id`"), "{}", m);
    ok(&mut a, "INSERT INTO tags (tag, label) VALUES ('a', 'Alpha'), ('b', 'Beta')");
    ok(&mut a, "ALTER TABLE tags ADD PRIMARY KEY (tag)");
    assert_eq!(rows(&mut a, "SELECT * FROM tags ORDER BY tag"), vec![vec!["a", "Alpha"], vec!["b", "Beta"]]);
}

#[test]
fn views_truncate_drop_grant() {
    let mut a = shop();
    ok(&mut a, "CREATE VIEW low_stock AS SELECT sku, name, qty FROM parts WHERE qty < 50");
    assert_eq!(col(&mut a, "SELECT sku FROM low_stock ORDER BY sku"), vec!["B-2", "N-2", "W-1"]);
    ok(&mut a, "UPDATE parts SET qty = 500 WHERE sku = 'B-2'");
    assert_eq!(col(&mut a, "SELECT sku FROM low_stock ORDER BY sku"), vec!["N-2", "W-1"], "a view reads live data");
    let full = rows(&mut a, "SHOW FULL TABLES");
    assert!(full.iter().any(|r| r[0] == "low_stock" && r[1] == "VIEW"));
    assert!(err(&mut a, "CREATE VIEW low_stock AS SELECT 1").contains("already exists"));
    ok(&mut a, "CREATE OR REPLACE VIEW low_stock AS SELECT sku FROM parts WHERE qty < 20");
    assert_eq!(col(&mut a, "SELECT * FROM low_stock ORDER BY 1"), vec!["N-2", "W-1"]);
    assert!(err(&mut a, "UPDATE low_stock SET sku = 'x'").contains("view"));
    ok(&mut a, "DROP VIEW low_stock");
    assert!(err(&mut a, "SELECT * FROM low_stock").contains("No table"));
    // the views table stays out of sight
    assert!(!col(&mut a, "SHOW TABLES").iter().any(|t| t.starts_with("_iqt")));
    // TRUNCATE is refused while another table points here
    assert!(err(&mut a, "TRUNCATE suppliers").contains("points to it"));
    ok(&mut a, "TRUNCATE TABLE parts");
    assert_eq!(one(&mut a, "SELECT COUNT(*) FROM parts"), "0");
    ok(&mut a, "RENAME TABLE parts TO items");
    ok(&mut a, "DROP TABLE items");
    assert!(err(&mut a, "DROP TABLE items").contains("No table"));
    ok(&mut a, "DROP TABLE IF EXISTS items");
    // privileges
    let w = crate::solana::b58(&crate::solana::Keypair::from_seed([7; 32]).pubkey);
    ok(&mut a, &format!("GRANT INSERT, UPDATE ON suppliers TO '{}'@'%'", w));
    assert!(!one(&mut a, "SHOW GRANTS FOR suppliers").is_empty());
    let tb = a.drafts[0].tables.iter().find(|t| t.title == "suppliers").unwrap().clone();
    assert_eq!(tb.writers, vec![w.clone()]);
    ok(&mut a, "GRANT INSERT ON suppliers TO PUBLIC");
    assert!(a.drafts[0].tables.iter().find(|t| t.title == "suppliers").unwrap().open);
    ok(&mut a, &format!("REVOKE ALL ON suppliers FROM '{}'; REVOKE INSERT ON suppliers FROM PUBLIC", w));
    let tb = a.drafts[0].tables.iter().find(|t| t.title == "suppliers").unwrap().clone();
    assert!(!tb.open && tb.writers.is_empty());
    assert!(err(&mut a, "GRANT INSERT ON suppliers TO 'bob'").contains("isn't a Solana wallet"));
}

#[test]
fn dump_and_import() {
    let mut a = shop();
    ok(&mut a, "CREATE VIEW v AS SELECT sku FROM parts");
    let dump = a.dump_sql("k", None, true, true);
    assert!(dump.contains("CREATE TABLE `suppliers`") && dump.contains("INSERT INTO `parts`") && dump.contains("CREATE OR REPLACE VIEW `v`"));
    // load it into a fresh database
    let mut b = db();
    let out = b.run_sql_opts("k", &dump, true);
    assert!(out.iter().all(|o| !matches!(o, Out::Msg(false, _))), "{:?}", out);
    assert_eq!(one(&mut b, "SELECT COUNT(*) FROM parts"), "5");
    assert_eq!(one(&mut b, "SELECT price FROM parts WHERE sku = 'N-1'"), "0.08");
    assert_eq!(one(&mut b, "SELECT COUNT(*) FROM v"), "5");
    assert!(err(&mut b, "INSERT INTO parts (sku, name, supplier_id) VALUES ('Z', 'z', 77)").contains("isn't in suppliers"), "keys came along");
    // phpMyAdmin's layout: keys added after the rows
    let mut c = db();
    let pma = "-- phpMyAdmin SQL Dump\n/*!40101 SET NAMES utf8mb4 */;\nSET SQL_MODE = \"NO_AUTO_VALUE_ON_ZERO\";\nSTART TRANSACTION;\n\
        CREATE TABLE `users` (\n  `id` int(11) NOT NULL,\n  `email` varchar(255) NOT NULL,\n  `created` timestamp NOT NULL DEFAULT current_timestamp()\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_general_ci;\n\
        INSERT INTO `users` (`id`, `email`, `created`) VALUES\n(1, 'a@x.com', '2026-01-01 10:00:00'),\n(2, 'b@x.com', '2026-01-02 11:30:00');\n\
        ALTER TABLE `users`\n  ADD PRIMARY KEY (`id`),\n  ADD UNIQUE KEY `email` (`email`);\n\
        ALTER TABLE `users`\n  MODIFY `id` int(11) NOT NULL AUTO_INCREMENT, AUTO_INCREMENT=3;\nCOMMIT;\n";
    let out = c.run_sql_opts("k", pma, true);
    assert!(out.iter().all(|o| !matches!(o, Out::Msg(false, _))), "{:?}", out);
    assert_eq!(rows(&mut c, "SHOW COLUMNS FROM users")[0], vec!["id", "INT", "NO", "PRI", "NULL", "auto_increment"]);
    ok(&mut c, "INSERT INTO users (email) VALUES ('c@x.com')");
    assert_eq!(one(&mut c, "SELECT MAX(id) FROM users"), "3");
    assert!(err(&mut c, "INSERT INTO users (email) VALUES ('A@x.com')").contains("unique"));
}

#[test]
fn structure_records_on_chain() {
    use crate::pack::{self, Record, Schema, SourcePack};
    use crate::schema::{ColMeta, Doc, Ty};
    // rows saved with keys (id, qty); then the owner renames qty → count, adds
    // a column with a default, and later re-keys by sku and truncates
    let data = |tx: &str, cols: &[&str], id: usize, vals: Vec<Vec<&str>>| SourcePack {
        id: tx.into(),
        tx: tx.into(),
        signer: "OWNER".into(),
        time: None,
        schema: Schema { cols: cols.iter().map(|c| c.to_string()).collect(), id },
        recs: vals.into_iter().map(|v| Record { vals: v.into_iter().map(|x| Json::Str(x.into())).collect(), deleted: false }).collect(),
        meta: None,
    };
    let doc = |cols: Vec<(&str, ColMeta)>, pk: &str, clear: bool| SourcePack {
        id: "s".into(),
        tx: "s".into(),
        signer: "OWNER".into(),
        time: None,
        schema: Schema { cols: vec!["id".into()], id: 0 },
        recs: vec![],
        meta: Some(
            Doc {
                cols: cols.into_iter().map(|(n, m)| (n.to_string(), m)).collect(),
                pk: pk.into(),
                keys: Default::default(),
                clear,
                dropped: false,
                snap: vec![],
            }
            .to_json(),
        ),
    };
    let p1 = data("t1", &["id", "sku", "qty"], 0, vec![vec!["1", "A", "5"], vec!["2", "B", "7"]]);
    let d1 = doc(
        vec![
            ("id", ColMeta::plain("id")),
            ("sku", ColMeta::plain("sku")),
            ("count", ColMeta::typed("qty", Ty::Int(crate::schema::IntKind::Int, false))),
            ("bin", ColMeta { fill: Json::Str("A1".into()), ..ColMeta::plain("bin") }),
        ],
        "id",
        false,
    );
    let (m, d) = pack::merge_events(&[p1.clone(), d1.clone()], &|s| s == "OWNER", &|_| true);
    let d = d.unwrap();
    let metas: Vec<ColMeta> = d.cols.iter().map(|c| c.1.clone()).collect();
    let v = crate::schema::align(&metas, &m[0].vals);
    assert_eq!(
        v,
        vec![Json::Str("1".into()), Json::Str("A".into()), Json::Num("5".into()), Json::Str("A1".into())],
        "renamed column keeps its values; the new one reads its fill"
    );
    // re-key by sku, then a record keyed by sku updates the right row
    let d2 = doc(vec![("id", ColMeta::plain("id")), ("sku", ColMeta::plain("sku"))], "sku", false);
    let p2 = data("t2", &["id", "sku", "qty"], 1, vec![vec!["2", "B", "70"]]);
    let (m, _) = pack::merge_events(&[p1.clone(), d2.clone(), p2.clone()], &|s| s == "OWNER", &|_| true);
    assert_eq!(m.len(), 2);
    assert_eq!(m.iter().find(|r| r.key == "B").unwrap().vals[2].1, Json::Str("70".into()));
    // structure records from anyone else are ignored
    let mut fake = doc(vec![("x", ColMeta::plain("x"))], "x", true);
    fake.signer = "MALLORY".into();
    let (m, d) = pack::merge_events(&[p1.clone(), fake], &|s| s == "OWNER", &|_| true);
    assert_eq!((m.len(), d.is_none()), (2, true));
    // TRUNCATE: rows before the record are gone, rows after it stay
    let d3 = doc(vec![("id", ColMeta::plain("id")), ("sku", ColMeta::plain("sku"))], "id", true);
    let p3 = data("t3", &["id", "sku"], 0, vec![vec!["9", "Z"]]);
    let (m, _) = pack::merge_events(&[p1, d3, p3], &|s| s == "OWNER", &|_| true);
    assert_eq!(m.iter().map(|r| r.key.clone()).collect::<Vec<_>>(), vec!["9"]);
    // the record round-trips through a pack payload
    let payload = pack::encode_schema(&d1.meta.clone().unwrap(), crate::iq::INLINE_CAP_V1);
    let (_, recs, meta) = pack::decode_any(&payload).unwrap();
    assert!(recs.is_empty() && Doc::from_json(&meta.unwrap()).unwrap().cols.len() == 4);
}

#[test]
fn saves_use_storage_keys() {
    let mut a = db();
    ok(&mut a, "CREATE TABLE t (code VARCHAR(10) PRIMARY KEY, qty INT); INSERT INTO t VALUES ('a', 1)");
    // pretend it was saved, then rename a column: the pack still says "qty"
    a.drafts[0].tables[0].created = Some("sig".into());
    for r in a.drafts[0].tables[0].rows.iter_mut() {
        r.sig = Some("s1".into());
    }
    a.ensure_base("k", 0);
    for tv in a.bases.values_mut() {
        tv.loading = false;
        tv.done = true;
    }
    ok(&mut a, "ALTER TABLE t RENAME COLUMN qty TO amount; UPDATE t SET amount = 2");
    assert!(a.drafts[0].tables[0].schema_changed());
    let cap = a.inline_cap();
    let plan = a.plan_for("k", 0, cap).clone().unwrap();
    let (schema, recs) = crate::pack::decode_payload(&plan[0].payload).unwrap();
    assert_eq!(schema.cols, vec!["code".to_string(), "qty".into()]);
    assert_eq!(recs[0].vals[1], Json::Num("2".into()));
    // DROP TABLE on a saved table is a pending, reversible change
    ok(&mut a, "DROP TABLE t");
    assert!(a.drafts[0].tables[0].dropped);
    ok(&mut a, "CREATE TABLE t (code VARCHAR(10) PRIMARY KEY, qty INT)");
    let tb = &a.drafts[0].tables[0];
    assert!(!tb.dropped && tb.clear, "re-created: saved rows will be cleared");
    assert!(tb.meta.iter().all(|m| m.key != "qty"), "old storage keys aren't reused");
}

#[test]
fn sheet_and_sql_share_rules() {
    let mut a = shop();
    // the sheet may leave required cells for later, but not break types or keys
    let t = a.tbl("k", "parts").unwrap();
    let rows = a.sheet_rows("k", t);
    let r = rows.iter().find(|r| r.vals[0].cell_text() == "B-1").unwrap().clone();
    let mut v = r.vals.clone();
    v[2] = Json::Str("many".into());
    let e = a.apply_changes(
        "k",
        t,
        vec![crate::constraints::Change::Update { row: r.clone(), vals: v, set: vec![true; 7] }],
        &crate::constraints::Opts { strict: false, fk_checks: true },
    );
    assert!(e.unwrap_err().contains("whole number"));
    let e = a.apply_changes(
        "k",
        t,
        vec![crate::constraints::Change::Insert { vals: vec![Json::Str("Q-1".into())], given: vec![true] }],
        &crate::constraints::Opts { strict: false, fk_checks: true },
    );
    assert!(e.is_ok(), "a half-filled row is fine in the sheet");
    assert!(a.row_problems("k", t).iter().any(|p| p.contains("name can't be empty")));
}

#[test]
fn screens_build_valid_sql() {
    use crate::ws_actions::type_from_form as ty;
    assert_eq!(ty("text", "").unwrap(), "VARCHAR(255)");
    assert_eq!(ty("text", "40").unwrap(), "VARCHAR(40)");
    assert_eq!(ty("decimal", "").unwrap(), "DECIMAL(15,2)");
    assert_eq!(ty("enum", "small, medium,large").unwrap(), "ENUM('small','medium','large')");
    assert_eq!(ty("any", "").unwrap(), "");
    assert_eq!(ty("custom", "smallint unsigned").unwrap(), "smallint unsigned");
    assert!(ty("custom", "NOPE(").is_err() && ty("enum", " , ").is_err());
    // what the column form and the other screens send is what the SQL tab accepts
    let mut a = db();
    ok(&mut a, "CREATE TABLE t (id INT AUTO_INCREMENT PRIMARY KEY, name VARCHAR(20), size TEXT)");
    ok(&mut a, &format!("ALTER TABLE `t` CHANGE `size` `size` {} DEFAULT 'small' COMMENT 'from the form'", ty("enum", "small, large").unwrap()));
    ok(&mut a, "ALTER TABLE `t` ADD COLUMN `n` INT NOT NULL DEFAULT 3 AFTER `name`");
    ok(&mut a, "INSERT INTO `t` (`name`) VALUES ('a')");
    assert_eq!(rows(&mut a, "SELECT name, n, size FROM t"), vec![vec!["a".to_string(), "3".into(), "small".into()]]);
    assert!(err(&mut a, "INSERT INTO `t` (`name`, `size`) VALUES ('b', 'huge')").contains("huge"));
    ok(&mut a, "GRANT INSERT ON `t` TO PUBLIC");
    assert!(a.drafts[0].tables[0].open);
    ok(&mut a, "RENAME TABLE `t` TO `things`");
    assert_eq!(rows(&mut a, "SELECT COUNT(*) FROM things"), vec![vec!["1".to_string()]]);
}

#[test]
fn checkpoints_and_chunked_writes() {
    use crate::pack::{self, Record, Schema, SourcePack};
    use crate::schema::{ColMeta, Doc};
    let schema = Schema { cols: vec!["id".into(), "v".into()], id: 0 };
    let data = |id: &str, signer: &str, vals: Vec<(&str, &str)>, dels: Vec<&str>| {
        let mut recs: Vec<Record> =
            vals.iter().map(|(k, v)| Record { vals: vec![Json::Str(k.to_string()), Json::Str(v.to_string())], deleted: false }).collect();
        recs.extend(dels.iter().map(|k| Record { vals: vec![Json::Str(k.to_string()), Json::Null], deleted: true }));
        SourcePack { id: id.into(), tx: id.into(), signer: signer.into(), time: None, schema: schema.clone(), recs, meta: None }
    };
    let ck = |signer: &str, snap: Vec<&str>| SourcePack {
        id: "ck".into(),
        tx: "ck".into(),
        signer: signer.into(),
        time: None,
        schema: Schema { cols: vec!["id".into()], id: 0 },
        recs: vec![],
        meta: Some(
            Doc {
                cols: vec![("id".into(), ColMeta::plain("id")), ("v".into(), ColMeta::plain("v"))],
                pk: "id".into(),
                keys: Default::default(),
                clear: false,
                dropped: false,
                snap: snap.into_iter().map(String::from).collect(),
            }
            .to_json(),
        ),
    };
    let history = vec![
        data("p1", "O", vec![("A", "1"), ("B", "2")], vec![]),
        data("x1", "U", vec![("E", "9")], vec![]),
        data("p2", "O", vec![("A", "5")], vec!["B"]),
        data("S", "O", vec![("A", "5"), ("C", "3")], vec![]),
        data("p3", "O", vec![("F", "6")], vec![]),
        ck("O", vec!["S"]),
        ck("U", vec!["x1"]),
        data("p4", "O", vec![("D", "4")], vec![]),
    ];
    let official = |s: &str| s == "O";
    let (m, doc) = pack::merge_events(&history, &official, &|_| true);
    let got: Vec<(String, String)> = m.iter().map(|r| (r.key.clone(), r.vals[1].1.cell_text())).collect();
    let want: Vec<(String, String)> = vec![("A".into(), "5".into()), ("C".into(), "3".into()), ("F".into(), "6".into()), ("D".into(), "4".into())];
    assert_eq!(got, want, "state = snapshot + what came after it; the unofficial checkpoint is ignored");
    assert!(doc.unwrap().snap.is_empty());
    // readers going newest-first can stop once the snapshot pack is in
    let newest_first: Vec<SourcePack> = history.iter().rev().cloned().collect();
    assert!(!pack::checkpoint_covers(&newest_first[..3], &official));
    assert!(pack::checkpoint_covers(&newest_first[..5], &official));
    // ... and get the same answer from just that part of history
    let part: Vec<SourcePack> = newest_first[..5].iter().rev().cloned().collect();
    let (m2, _) = pack::merge_events(&part, &official, &|_| true);
    assert_eq!(m2.iter().map(|r| r.key.clone()).collect::<Vec<_>>(), vec!["A", "C", "F", "D"]);
    // a checkpoint whose snapshot pack is missing is not trusted: full replay
    let broken: Vec<SourcePack> = history.iter().filter(|p| p.id != "S").cloned().collect();
    let (m3, _) = pack::merge_events(&broken, &official, &|_| true);
    assert!(m3.iter().any(|r| r.key == "E"), "no valid checkpoint: everything replays");

    // cheapest write: 1 pack direct; many packs → one chunked pack
    let recs: Vec<Record> = (0..3000)
        .map(|i| Record { vals: vec![Json::Str(format!("K{:05}", i)), Json::Str(format!("value {} {}", i * 7919 % 10007, i * 31))], deleted: false })
        .collect();
    let small = pack::plan_best(&schema, &recs[..5], crate::iq::INLINE_CAP_V1, crate::iq::CHUNK_SIZE_V1, true).unwrap();
    assert_eq!((small.len(), small[0].chunks), (1, 0));
    let direct = pack::plan(&schema, &recs, crate::iq::INLINE_CAP_V1, true).unwrap();
    let best = pack::plan_best(&schema, &recs, crate::iq::INLINE_CAP_V1, crate::iq::CHUNK_SIZE_V1, true).unwrap();
    assert!(
        direct.len() >= 4 && best.len() == 1 && best[0].chunks >= 2,
        "{} direct packs vs {:?}",
        direct.len(),
        best.iter().map(|p| p.chunks).collect::<Vec<_>>()
    );
    assert!(best[0].cost() < direct.iter().map(|p| p.cost()).sum::<u64>());
    // a record bigger than one transaction is fine now
    let mut x: u64 = 88172645463325252;
    let noise: String = (0..20000)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            char::from(b'a' + (x % 26) as u8)
        })
        .collect();
    let huge = vec![Record { vals: vec![Json::Str("big".into()), Json::Str(noise)], deleted: false }];
    assert!(pack::plan(&schema, &huge, crate::iq::INLINE_CAP_V1, true).is_err());
    let hp = pack::plan_best(&schema, &huge, crate::iq::INLINE_CAP_V1, crate::iq::CHUNK_SIZE_V1, true).unwrap();
    assert_eq!(hp.len(), 1);
    assert!(hp[0].chunks >= 2);
    let back = pack::decode_payload(&hp[0].payload).unwrap();
    assert_eq!(back.1, huge);

    // OPTIMIZE TABLE on a saved table: the next save rewrites every row
    let mut a = db();
    ok(&mut a, "CREATE TABLE t (id INT PRIMARY KEY, v TEXT)");
    ok(&mut a, "INSERT INTO t VALUES (1, 'a'), (2, 'b')");
    a.drafts[0].tables[0].created = Some("sig".into());
    for r in a.drafts[0].tables[0].rows.iter_mut() {
        r.sig = Some("s1".into());
    }
    a.ensure_base("k", 0);
    for tv in a.bases.values_mut() {
        tv.loading = false;
        tv.done = true;
    }
    let cap = a.inline_cap();
    assert!(a.plan_for("k", 0, cap).as_ref().unwrap().is_empty());
    let m = ok(&mut a, "OPTIMIZE TABLE t");
    assert!(a.drafts[0].tables[0].checkpoint, "{}", m);
    let plan = a.plan_for("k", 0, cap).clone().unwrap();
    assert_eq!(plan.iter().map(|p| p.count).sum::<usize>(), 2, "every live row is rewritten");
    assert!(a.drafts[0].tables[0].schema_changed());
}

#[test]
fn upload_profiles() {
    // IQ's upload profiles
    assert_eq!(crate::upload::profile("medium").parallel, 5);
    assert_eq!(crate::upload::profile("nonsense").name, crate::upload::DEFAULT_PROFILE);
}
