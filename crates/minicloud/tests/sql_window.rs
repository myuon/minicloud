//! An app's SQL store, read from the admin app (issue 88): the `host`
//! module's `sqlTables` and `sqlQuery` ([`minicloud::sql::peek_tables`] and
//! [`minicloud::sql::peek_query`]), and the admin app's page over them.
//!
//! The module's own window is the `control` fixture as the app `admin`, as
//! in tests/admin.rs: one path an operation, the answer as text. The pages
//! are `apps/admin` itself, over the apps beside it. The app with a store is
//! `tests/apps/catalog`, the one the `sql` tests are written on.

mod common;

use common::*;
use minicloud::Host;

/// The admin app's hostname in the module's own tests.
const ADMIN_HOST: &str = "admin.example";

const CONTROL_TOML: &str = "grant = [\"admin\"]\n\n[limits]\ndeadline = \"60s\"\n\n\
[route]\nhosts = [\"admin.example\"]\n";

/// The control fixture as the app `admin`, reached by a hostname of its own
/// so that the apps beside it keep their paths.
fn control() -> AppSpec<'static> {
    AppSpec {
        name: "admin",
        from: fixtures().join("control"),
        config: Some(CONTROL_TOML),
    }
}

/// The `catalog` fixture: a table of books, and the app the window is
/// pointed at.
fn catalog() -> AppSpec<'static> {
    AppSpec {
        name: "catalog",
        from: fixtures().join("catalog"),
        config: None,
    }
}

/// `/setup`, `/index` and the books, as tests/sql.rs stocks it.
fn stocked(host: &Host) {
    assert_eq!(get(host.addr, "/catalog/setup").status, 200);
    assert_eq!(get(host.addr, "/catalog/index").status, 200);
    for (title, pages) in [("Dune", "412"), ("Ubik", "224"), ("Solaris", "204")] {
        let added = get(
            host.addr,
            &format!("/catalog/add?title={title}&pages={pages}"),
        );
        assert_eq!(added.status, 200, "{}", added.body);
    }
}

/// One operation of the control fixture.
fn op(host: &Host, path: &str) -> Answer {
    send_raw(
        host.addr,
        format!("GET {path} HTTP/1.1\r\nHost: {ADMIN_HOST}\r\nConnection: close\r\n\r\n")
            .as_bytes(),
    )
}

/// `host.sqlTables(app)` as the fixture prints it: one line a table,
/// `name | rows | counted`.
fn tables(host: &Host, app: &str) -> Vec<String> {
    let answer = op(host, &format!("/sqlTables?app={app}"));
    assert_eq!(answer.status, 200, "{}", answer.body);
    answer
        .body
        .lines()
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

/// `host.sqlQuery(app, statement)`: the columns and then a line a row, or
/// the `Err`'s message.
fn query(host: &Host, app: &str, statement: &str) -> Result<Vec<String>, String> {
    let answer = op(
        host,
        &format!("/sqlQuery?app={app}&q={}", urlencode(statement)),
    );
    if answer.status != 200 {
        return Err(answer.body.trim().to_string());
    }
    Ok(answer
        .body
        .lines()
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect())
}

/// The message of a statement that was refused.
fn refused(host: &Host, app: &str, statement: &str) -> String {
    match query(host, app, statement) {
        Err(why) => why,
        Ok(answered) => panic!("`{statement}` was answered: {answered:?}"),
    }
}

/// A query value, percent-encoded: a statement holds spaces, quotes and
/// worse.
fn urlencode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

#[test]
fn the_window_lands_on_the_tables_and_their_row_counts() {
    let apps = apps(&[control(), catalog()]);
    let host = start(&apps, 2);
    // Before anything is written there is a file but no table in it.
    assert_eq!(tables(&host, "catalog"), Vec::<String>::new());
    stocked(&host);
    // `/index` made an index, which is not a table and is not in the list.
    assert_eq!(tables(&host, "catalog"), ["books | 3 | true"]);
}

#[test]
fn a_statement_answers_its_columns_and_its_rows() {
    let apps = apps(&[control(), catalog()]);
    let host = start(&apps, 2);
    stocked(&host);
    assert_eq!(
        query(
            &host,
            "catalog",
            "SELECT title, pages FROM books ORDER BY pages DESC"
        ),
        Ok(vec![
            "title|pages".to_string(),
            "'Dune'|'412'".to_string(),
            "'Ubik'|'224'".to_string(),
            "'Solaris'|'204'".to_string(),
        ])
    );
    // An aggregate, which is the question a window like this is opened for.
    assert_eq!(
        query(&host, "catalog", "SELECT sum(pages) FROM books"),
        Ok(vec!["sum(pages)".to_string(), "'840'".to_string()])
    );
}

/// The window rests on `sql.Store::query`'s refusal rather than a check of
/// its own, so every kind of write is refused where it is prepared: a write
/// to a table, a change of the schema, and the statements the authorizer
/// turns down, which a read-only connection would otherwise allow.
#[test]
fn every_statement_that_writes_or_is_not_allowed_is_refused() {
    let apps = apps(&[control(), catalog()]);
    let host = start(&apps, 2);
    stocked(&host);
    for statement in [
        "INSERT INTO books (title, pages) VALUES ('Nope', 1)",
        "UPDATE books SET title = 'Nope'",
        "DELETE FROM books",
        "CREATE TABLE sneaked (v TEXT)",
        "DROP TABLE books",
        "ALTER TABLE books RENAME TO shelf",
    ] {
        let why = refused(&host, "catalog", statement);
        assert!(why.contains("only reads"), "{statement}: {why}");
    }
    for statement in [
        "ATTACH DATABASE 'elsewhere.sqlite3' AS other",
        "PRAGMA journal_mode",
        "PRAGMA database_list",
        "BEGIN",
        "SAVEPOINT s",
    ] {
        let why = refused(&host, "catalog", statement);
        assert!(why.contains("not authorized"), "{statement}: {why}");
    }
    // Two statements in one are refused, so a `SELECT` cannot carry a
    // `DELETE` behind a `;`.
    let why = refused(&host, "catalog", "SELECT 1; DELETE FROM books");
    assert!(why.contains("one statement per call"), "{why}");

    // Nothing of any of it happened, and the app still works.
    assert_eq!(tables(&host, "catalog"), ["books | 3 | true"]);
    assert_eq!(
        get(host.addr, "/catalog/every").body.lines().count(),
        4,
        "a header and three books"
    );
}

#[test]
fn an_app_with_no_store_has_no_tables_and_a_statement_is_told_so() {
    let apps = apps(&[control(), catalog(), sample("notes")]);
    let host = start(&apps, 2);
    // `notes` is granted `kv` and not `sql`, so the host keeps no file for
    // it — and looking must not make one.
    assert_eq!(tables(&host, "notes"), Vec::<String>::new());
    let why = refused(&host, "notes", "SELECT 1");
    assert!(why.contains("no sql store"), "{why}");
    assert!(!apps.data.join("notes/sql.sqlite3").exists());

    // An app the host does not have names nothing at all.
    assert_eq!(tables(&host, "nowhere"), Vec::<String>::new());
    let why = refused(&host, "nowhere", "SELECT 1");
    assert!(why.contains("no app `nowhere`"), "{why}");
    // Nor does a name that is not an app's: it is not joined to a path.
    assert_eq!(tables(&host, "../catalog"), Vec::<String>::new());
}

#[test]
fn an_answer_past_the_windows_rows_says_that_it_is() {
    let apps = apps(&[control(), catalog()]);
    let host = start(&apps, 2);
    stocked(&host);
    let rows = |n: u64| {
        format!(
            "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x<{n}) \
             SELECT x FROM c"
        )
    };
    let limit = minicloud::sql::PEEK_ROWS;
    // The columns and then one line a row: the whole of it, at the limit.
    assert_eq!(
        query(&host, "catalog", &rows(limit)).map(|lines| lines.len() as u64),
        Ok(limit + 1)
    );
    let why = refused(&host, "catalog", &rows(limit + 1));
    assert!(why.contains(&format!("max_rows of {limit}")), "{why}");
    // And whose limit it is: the app's `[sql]` is not the thing to raise.
    assert!(why.contains("the admin window's"), "{why}");
}

#[test]
fn a_null_is_not_an_empty_string_in_the_window() {
    let apps = apps(&[control(), catalog()]);
    let host = start(&apps, 2);
    stocked(&host);
    // The fixture quotes a value and leaves a NULL unquoted, so the two
    // cannot be read as the same cell.
    assert_eq!(
        query(&host, "catalog", "SELECT NULL AS a, '' AS b, 'x' AS c"),
        Ok(vec!["a|b|c".to_string(), "NULL|''|'x'".to_string()])
    );
    // The aggregate over no rows is a real NULL of the app's own data.
    assert_eq!(
        query(
            &host,
            "catalog",
            "SELECT max(pages) FROM books WHERE title = 'none'"
        ),
        Ok(vec!["max(pages)".to_string(), "NULL".to_string()])
    );
}

/// The window goes through the store the app has open, which is the only way
/// it can show what is there: a second connection on a WAL database reads
/// the snapshot it started with, so a row written a moment ago would be
/// missing from it.
#[test]
fn a_row_the_app_has_just_written_is_in_the_window() {
    let apps = apps(&[control(), catalog()]);
    let host = start(&apps, 2);
    stocked(&host);
    let added = get(host.addr, "/catalog/add?title=Fresh&pages=1");
    assert_eq!(added.status, 200, "{}", added.body);
    assert_eq!(
        query(
            &host,
            "catalog",
            "SELECT title FROM books WHERE title = 'Fresh'"
        ),
        Ok(vec!["title".to_string(), "'Fresh'".to_string()])
    );
    assert_eq!(tables(&host, "catalog"), ["books | 4 | true"]);
}

// ------------------------------------------------- the admin app's pages

const UI_HOST: &str = "covtools-admin.ramda.io";
/// `Basic base64("owner:ui-secret")`.
const UI_LOGIN: &str = "Basic b3duZXI6dWktc2VjcmV0";

/// `apps/admin`'s own config, with the secret as a value rather than from
/// the environment.
fn admin_app_toml() -> String {
    std::fs::read_to_string(bundled().join("admin/app.toml"))
        .unwrap()
        .replace("{ env = \"ADMIN_UI_TOKEN\" }", "{ value = \"ui-secret\" }")
}

fn ui_apps() -> Apps {
    let config: &'static str = Box::leak(admin_app_toml().into_boxed_str());
    apps(&[sample_with("admin", config), catalog(), sample("notes")])
}

/// A page of the admin app.
fn page(host: &Host, path: &str) -> Answer {
    send_raw(
        host.addr,
        format!(
            "GET {path} HTTP/1.1\r\nHost: {UI_HOST}\r\nConnection: close\r\n\
             Authorization: {UI_LOGIN}\r\n\r\n"
        )
        .as_bytes(),
    )
}

/// The page with `statement` typed into its box.
fn asked(host: &Host, app: &str, statement: &str) -> Answer {
    let answer = page(host, &format!("/apps/{app}/sql?q={}", urlencode(statement)));
    assert_eq!(answer.status, 200, "{}", answer.body);
    answer
}

#[test]
fn the_page_lands_on_the_tables_and_answers_a_statement() {
    let apps = ui_apps();
    let host = start(&apps, 2);
    stocked(&host);

    let landing = page(&host, "/apps/catalog/sql");
    assert_eq!(landing.status, 200, "{}", landing.body);
    assert!(
        landing.body.contains("<code>books</code>"),
        "{}",
        landing.body
    );
    assert!(landing.body.contains(">3</td>"), "{}", landing.body);
    // The page says what it will not do, and what it is bounded by.
    assert!(landing.body.contains("only reads"), "{}", landing.body);
    assert!(landing.body.contains("200 rows"), "{}", landing.body);
    // Each table carries the statement for its first rows.
    assert!(
        landing
            .body
            .contains("sql?q=SELECT%20%2A%20FROM%20%22books%22%20LIMIT%2050"),
        "{}",
        landing.body
    );

    // The app's tabs are the way here: a tab for the store it has.
    let app = page(&host, "/apps/catalog");
    assert!(
        app.body.contains("<a href=\"/apps/catalog/sql\">SQL</a>"),
        "{}",
        app.body
    );
    // The store is a card on the Overview, with what it holds.
    assert!(app.body.contains("class=store"), "{}", app.body);
    // And an app not granted `sql` has no such tab: the page on the other
    // end of one would say only that the host keeps no file for it.
    let notes = page(&host, "/apps/notes");
    assert!(!notes.body.contains("/apps/notes/sql"), "{}", notes.body);

    // One statement, as a table of its columns and its rows.
    let answered = asked(
        &host,
        "catalog",
        "SELECT title, pages FROM books ORDER BY title",
    );
    assert!(
        answered.body.contains("<th>title</th>"),
        "{}",
        answered.body
    );
    assert!(
        answered.body.contains("<code>Dune</code>"),
        "{}",
        answered.body
    );
    assert!(answered.body.contains("3 rows."), "{}", answered.body);
    // A NULL is said in words, and so is the empty string.
    let null = asked(&host, "catalog", "SELECT NULL AS a, '' AS b");
    assert!(null.body.contains(">NULL<"), "{}", null.body);
    assert!(null.body.contains(">empty<"), "{}", null.body);

    // A statement that writes comes back as the host's refusal.
    let write = asked(&host, "catalog", "DELETE FROM books");
    assert!(write.body.contains("Not answered."), "{}", write.body);
    assert!(write.body.contains("only reads"), "{}", write.body);
    assert_eq!(get(host.addr, "/catalog/every").body.lines().count(), 4);

    // An app that is not granted `sql` says so instead of showing a box.
    let none = page(&host, "/apps/notes/sql");
    assert_eq!(none.status, 200, "{}", none.body);
    assert!(none.body.contains("is not granted"), "{}", none.body);
    assert!(!none.body.contains("name=q"), "{}", none.body);

    // The page reads, whatever is posted to it.
    let posted = send_raw(
        host.addr,
        format!(
            "POST /apps/catalog/sql HTTP/1.1\r\nHost: {UI_HOST}\r\nConnection: close\r\n\
             Authorization: {UI_LOGIN}\r\nOrigin: https://{UI_HOST}\r\n\
             Sec-Fetch-Site: same-origin\r\nContent-Length: 0\r\n\r\n"
        )
        .as_bytes(),
    );
    assert_eq!(posted.status, 404, "{}", posted.body);
}

/// A window on somebody's data shows whatever is in it, and the statement
/// that asked for it goes back into the form's `value=`, inside quotes. Every
/// one of those is escaped by the `html"…"` hole it goes in, or this page
/// would be the way into the admin app.
#[test]
fn the_page_escapes_the_statement_the_values_and_the_error() {
    let apps = ui_apps();
    let host = start(&apps, 2);
    assert_eq!(get(host.addr, "/catalog/setup").status, 200);
    let title = "<img src=y>&\"'";
    let added = get(
        host.addr,
        &format!("/catalog/add?title={}&pages=1", urlencode(title)),
    );
    assert_eq!(added.status, 200, "{}", added.body);

    // A value of the app's own data, in a cell.
    let answered = asked(&host, "catalog", "SELECT title FROM books");
    assert!(!answered.body.contains("<img src=y>"), "{}", answered.body);
    assert!(
        answered.body.contains("&lt;img src=y&gt;&amp;&quot;&#39;"),
        "{}",
        answered.body
    );

    // The statement itself, back inside the attribute's quotes. A quote that
    // survived would end the attribute and leave the rest as markup.
    let typed = "SELECT * FROM \"<img src=y>\"";
    let bad = asked(&host, "catalog", typed);
    assert!(
        bad.body
            .contains("value=\"SELECT * FROM &quot;&lt;img src=y&gt;&quot;\""),
        "{}",
        bad.body
    );
    assert!(!bad.body.contains("<img src=y>"), "{}", bad.body);
    // And the host's message, which quotes the statement back.
    assert!(bad.body.contains("Not answered."), "{}", bad.body);
    assert!(
        bad.body.contains("no such table: &lt;img src=y&gt;"),
        "{}",
        bad.body
    );

    // A column name is the host's word too, and it labels a cell.
    let named = asked(&host, "catalog", "SELECT 1 AS \"<img src=z>\"");
    assert!(!named.body.contains("<img src=z>"), "{}", named.body);
    assert!(named.body.contains("&lt;img src=z&gt;"), "{}", named.body);
}

/// A cell longer than the column is cut where the store's page cuts a
/// value, and says that it was.
#[test]
fn the_page_cuts_a_long_cell_and_says_so() {
    let apps = ui_apps();
    let host = start(&apps, 2);
    assert_eq!(get(host.addr, "/catalog/setup").status, 200);
    let long = "x".repeat(400);
    assert_eq!(
        get(host.addr, &format!("/catalog/add?title={long}&pages=1")).status,
        200
    );
    let answered = asked(&host, "catalog", "SELECT title FROM books");
    assert!(
        answered.body.contains(&"x".repeat(140)),
        "{}",
        answered.body
    );
    assert!(
        !answered.body.contains(&"x".repeat(141)),
        "{}",
        answered.body
    );
    assert!(answered.body.contains("…"), "{}", answered.body);
}
