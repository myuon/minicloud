//! An app's queues, read from the admin app (issue 90): the `host` module's
//! `queues`, `queueMessages` and `queueDead`
//! ([`minicloud::queue::peek_depths`] and the two listings beside it), and
//! the admin app's page over them.
//!
//! The module's own window is the `control` fixture as the app `admin`, as
//! in tests/admin.rs: one path an operation, the answer as text. The pages
//! are `apps/admin` itself, over the apps beside it. The app with queues is
//! `tests/apps/post`, the one the queue tests are written on.
//!
//! A message is put on the store directly where a test needs one to stay
//! there: the delivery loop takes what is due within the second, and what
//! this window is for is the messages that are *not* going anywhere yet.

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

/// The `post` fixture with the `[[queues]]` a test needs: `/send` puts a
/// message on `mail`, and the consumer refuses a body of `fail`.
fn post(queues: &str) -> AppSpec<'static> {
    let config: &'static str = Box::leak(
        format!("grant = [\"queue\", \"kv\", \"log\", \"timer\"]\n{queues}").into_boxed_str(),
    );
    AppSpec {
        name: "post",
        from: fixtures().join("post"),
        config: Some(config),
    }
}

/// Two queues: one the tests use, one declared and never sent to.
const TWO: &str = "[[queues]]\nname = \"mail\"\nmax_attempts = 2\nretry = \"3600s\"\n\
lease = \"120s\"\nmax_depth = 500\nbatch = 4\n\n[[queues]]\nname = \"spare\"\n";

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs() as i64)
}

/// The store of `app`'s queues, the one the host has open where it has one.
fn store(apps: &Apps, app: &str) -> std::sync::Arc<std::sync::Mutex<minicloud::queue::Queues>> {
    minicloud::queue::shared(&apps.data.join(app).join("queue.sqlite3")).unwrap()
}

/// Puts a message on `app`'s queue, as its own `queue.send` would, and
/// answers its id. `max_depth` here is the test's, not the app's: what the
/// window shows of a message does not depend on which limit let it in.
fn put(apps: &Apps, app: &str, queue: &str, body: &str) -> i64 {
    let store = store(apps, app);
    let store = store.lock().unwrap();
    let at = now();
    store.send(queue, body, at, at, 64 << 10, 1 << 20).unwrap()
}

/// Buries a message with a reason of the test's own, where a reason the
/// consumer wrote would not do. No delivery has leased it (the tests that
/// bury have `post` disabled), and the store settles only what a delivery
/// holds, so the test moves the row itself, in one transaction, as
/// `Queues::bury` would: with the attempts it has, which is what the page
/// shows.
fn bury(apps: &Apps, app: &str, id: i64, why: &str) {
    let mut conn = rusqlite::Connection::open(apps.data.join(app).join("queue.sqlite3")).unwrap();
    let tx = conn.transaction().unwrap();
    let moved = tx
        .execute(
            "INSERT INTO dead (id, queue, body, attempts, due, sent, why, buried) \
             SELECT id, queue, body, attempts, due, sent, ?2, ?3 FROM message WHERE id = ?1",
            rusqlite::params![id, why, now()],
        )
        .unwrap();
    assert_eq!(moved, 1, "message {id} is on the queue");
    tx.execute("DELETE FROM message WHERE id = ?1", [id])
        .unwrap();
    tx.commit().unwrap();
}

/// One operation of the control fixture.
fn op(host: &Host, path: &str) -> Answer {
    send_raw(
        host.addr,
        format!("GET {path} HTTP/1.1\r\nHost: {ADMIN_HOST}\r\nConnection: close\r\n\r\n")
            .as_bytes(),
    )
}

/// The lines of one operation's answer.
fn lines(host: &Host, path: &str) -> Vec<String> {
    let answer = op(host, path);
    assert_eq!(answer.status, 200, "{}", answer.body);
    answer
        .body
        .lines()
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

/// `host.queues(app)`: one line a queue.
fn queues(host: &Host, app: &str) -> Vec<String> {
    lines(host, &format!("/queues?app={app}"))
}

/// `host.queueMessages(app, n)`: one line a message.
fn waiting(host: &Host, app: &str) -> Vec<String> {
    lines(host, &format!("/queueMessages?app={app}&n=1000"))
}

/// `host.queueDead(app, n)`.
fn dead(host: &Host, app: &str) -> Vec<String> {
    lines(host, &format!("/queueDead?app={app}&n=1000"))
}

/// Stops routing to `app`, so that nothing on its queues is delivered while
/// a test looks at them.
fn disable(host: &Host, app: &str) {
    let answer = op(host, &format!("/enable?app={app}&on=false"));
    assert_eq!(answer.status, 200, "{}", answer.body);
}

fn enable(host: &Host, app: &str) {
    let answer = op(host, &format!("/enable?app={app}&on=true"));
    assert_eq!(answer.status, 200, "{}", answer.body);
}

/// A queue declared and never used is listed as empty: 0 is an answer, and
/// a queue that was left out of the list would read as one the app does not
/// have.
#[test]
fn every_declared_queue_is_listed_with_its_settings() {
    let apps = apps(&[control(), post(TWO)]);
    let host = start(&apps, 2);
    let listed = queues(&host, "post");
    assert_eq!(listed.len(), 2, "{listed:?}");
    assert!(
        listed[0].starts_with("mail | /queue/mail | due=0 held=0 dead=0 | declared=true"),
        "{listed:?}"
    );
    // The settings are the host's own, which is where `[[queues]]`'s
    // defaults have been applied: a retry of an hour as written, a lease of
    // two minutes, and the path a queue takes when it names none.
    assert!(
        listed[0].contains("depth=500 attempts=2 retry=3600000 lease=120000 batch=4 body=65536"),
        "{listed:?}"
    );
    assert!(
        listed[1].starts_with("spare | /queue/spare | due=0 held=0 dead=0 | declared=true"),
        "{listed:?}"
    );
    assert_eq!(waiting(&host, "post"), Vec::<String>::new());
    assert_eq!(dead(&host, "post"), Vec::<String>::new());
}

/// The window is a read. A page that leased what it showed would take the
/// message the next delivery was about to take, which is the one thing it
/// must not do — so this looks at a message several times and then lets the
/// delivery happen.
#[test]
fn a_waiting_message_is_shown_and_reading_it_takes_nothing() {
    let apps = apps(&[control(), post(TWO)]);
    let host = start(&apps, 2);
    disable(&host, "post");
    let id = put(&apps, "post", "mail", "hello");

    for _ in 0..3 {
        let listed = waiting(&host, "post");
        assert_eq!(listed.len(), 1, "{listed:?}");
        // Never tried: `attempts` is what a lease would have raised.
        assert!(
            listed[0].starts_with(&format!("{id} | mail | due | attempts=0 | hello |")),
            "{listed:?}"
        );
        assert!(listed[0].contains("bytes=5 cut=false"), "{listed:?}");
        // Buried at nothing, and no reason: it is not a dead letter.
        assert!(listed[0].contains("buried=0 | "), "{listed:?}");
        assert!(
            queues(&host, "post")[0].contains("due=1 held=0 dead=0"),
            "{:?}",
            queues(&host, "post")
        );
    }

    // And the delivery that follows is the delivery that would have
    // happened, which is what "reads only" has to mean here.
    enable(&host, "post");
    wait_until("the message to be delivered", || {
        count(&host, "post", "queue_delivered") == 1
    });
    assert_eq!(get(host.addr, "/post/taken").body.trim(), "hello;");
    assert_eq!(waiting(&host, "post"), Vec::<String>::new());
    assert!(
        queues(&host, "post")[0].contains("due=0 held=0 dead=0"),
        "{:?}",
        queues(&host, "post")
    );
}

/// A message whose due time is ahead is `held`, and the store says no more
/// than that: `lease` pushes `due` out by the lease and `retry` pushes it
/// out by the delay, into the same column. This one is a failed delivery
/// waiting an hour; a delivery in flight looks exactly the same, which
/// `queue::tests` checks where it can hold the clock still.
#[test]
fn a_message_waiting_for_its_retry_is_held_and_says_when_it_is_due() {
    let apps = apps(&[control(), post(TWO)]);
    let host = start(&apps, 2);
    assert_eq!(get(host.addr, "/post/send?body=fail").status, 202);
    wait_until("the first attempt", || {
        count(&host, "post", "queue_retried") == 1
    });
    let listed = waiting(&host, "post");
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert!(
        listed[0].contains("| mail | held | attempts=1 | fail |"),
        "{listed:?}"
    );
    assert!(
        queues(&host, "post")[0].contains("due=0 held=1 dead=0"),
        "{:?}",
        queues(&host, "post")
    );
    // When it is due: an hour off, as `retry` says, said in milliseconds
    // like every other time this module carries.
    let due: i64 = listed[0]
        .split("due=")
        .nth(1)
        .and_then(|rest| rest.split(' ').next())
        .and_then(|number| number.parse().ok())
        .unwrap_or_else(|| panic!("a due time: {listed:?}"));
    let in_an_hour = (now() + 3600) * 1000;
    assert!(
        (due - in_an_hour).abs() < 60_000,
        "{due} is not about {in_an_hour}"
    );
}

#[test]
fn a_dead_letter_is_listed_with_why_it_was_buried() {
    let apps = apps(&[
        control(),
        post("[[queues]]\nname = \"mail\"\nmax_attempts = 1\nretry = \"3600s\"\n"),
    ]);
    let host = start(&apps, 2);
    assert_eq!(get(host.addr, "/post/send?body=fail").status, 202);
    wait_until("the dead letter", || {
        count(&host, "post", "queue_buried") == 1
    });
    let listed = dead(&host, "post");
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert!(
        listed[0].contains("| mail | dead | attempts=1 | fail |"),
        "{listed:?}"
    );
    // The consumer's own words: the status and the first line it answered.
    assert!(
        listed[0].ends_with("| 500 this consumer refuses"),
        "{listed:?}"
    );
    assert!(!listed[0].contains("buried=0 "), "{listed:?}");
    // It is not waiting, and the counts keep the two apart.
    assert_eq!(waiting(&host, "post"), Vec::<String>::new());
    assert!(
        queues(&host, "post")[0].contains("due=0 held=0 dead=1"),
        "{:?}",
        queues(&host, "post")
    );
}

/// A queue the app no longer declares keeps whatever was on it and nothing
/// will ever deliver it, which is exactly the thing a window like this is
/// for. It is listed after the declared ones, saying so.
#[test]
fn a_queue_the_app_no_longer_declares_is_listed_saying_so() {
    let apps = apps(&[control(), post(TWO)]);
    let host = start(&apps, 2);
    disable(&host, "post");
    put(&apps, "post", "gone", "orphan");
    let listed = queues(&host, "post");
    assert_eq!(listed.len(), 3, "{listed:?}");
    assert!(
        listed[2].starts_with("gone |  | due=1 held=0 dead=0 | declared=false"),
        "{listed:?}"
    );
    // No settings, because it has none: they came from `[[queues]]`.
    assert!(
        listed[2].contains("depth=0 attempts=0 retry=0 lease=0 batch=0 body=0"),
        "{listed:?}"
    );
    // And the message is in the listing, with the queue it is on.
    assert!(
        waiting(&host, "post")[0].contains("| gone | due | attempts=0 | orphan |"),
        "{:?}",
        waiting(&host, "post")
    );
}

#[test]
fn an_app_with_no_queue_has_nothing_and_no_file_is_made_for_it() {
    let apps = apps(&[control(), post(TWO), sample("notes")]);
    let host = start(&apps, 2);
    // `notes` is granted `kv` and not `queue`, so the host keeps no file
    // for it — and looking must not make one.
    assert_eq!(queues(&host, "notes"), Vec::<String>::new());
    assert_eq!(waiting(&host, "notes"), Vec::<String>::new());
    assert_eq!(dead(&host, "notes"), Vec::<String>::new());
    assert!(!apps.data.join("notes/queue.sqlite3").exists());

    // An app the host does not have names nothing at all.
    assert_eq!(queues(&host, "nowhere"), Vec::<String>::new());
    assert_eq!(waiting(&host, "nowhere"), Vec::<String>::new());
    // Nor does a name that is not an app's: it is not joined to a path.
    assert_eq!(queues(&host, "../post"), Vec::<String>::new());
    assert_eq!(waiting(&host, "../post"), Vec::<String>::new());
}

/// The listing stops at the window's own limit and says nothing of what is
/// past it; the counts beside it are whole, which is how the page says how
/// much it is not showing.
#[test]
fn the_listing_stops_at_the_windows_limit_and_the_counts_do_not() {
    let apps = apps(&[control(), post(TWO)]);
    let host = start(&apps, 2);
    disable(&host, "post");
    let more = minicloud::queue::PEEK_MESSAGES + 5;
    for n in 0..more {
        put(&apps, "post", "mail", &format!("message {n}"));
    }
    assert_eq!(
        waiting(&host, "post").len() as i64,
        minicloud::queue::PEEK_MESSAGES
    );
    // Asking for fewer answers fewer; asking for more does not raise it.
    assert_eq!(lines(&host, "/queueMessages?app=post&n=3").len(), 3);
    assert!(
        queues(&host, "post")[0].contains(&format!("due={more} held=0 dead=0")),
        "{:?}",
        queues(&host, "post")
    );
}

/// One message of a queue carries a payload, which is routinely longer than
/// a listing's 512 characters — so the window has a read for one message,
/// as the `kv` window has for one key.
#[test]
fn one_message_is_read_whole_by_id() {
    let apps = apps(&[control(), post(TWO)]);
    let host = start(&apps, 2);
    disable(&host, "post");
    let long = "p".repeat(2000);
    let id = put(&apps, "post", "mail", &long);

    // The listing cut it; the message's own read did not.
    let listed = waiting(&host, "post");
    assert!(listed[0].contains("cut=true"), "{listed:?}");
    let one = lines(&host, &format!("/queueMessage?app=post&id={id}"));
    assert_eq!(one.len(), 1, "{one:?}");
    assert!(one[0].contains(&long), "the whole body");
    assert!(one[0].contains("cut=false"), "{one:?}");
    assert!(one[0].contains("| mail | due | attempts=0 |"), "{one:?}");

    // An id no queue of this app holds is an `Err`, not an error of the
    // host's: a link to a message that has since been delivered is an
    // ordinary thing to click.
    let gone = op(&host, "/queueMessage?app=post&id=9999");
    assert_eq!(gone.status, 404, "{}", gone.body);
    assert!(gone.body.contains("no message 9999"), "{}", gone.body);
    // And so is any id of an app that has no queue at all.
    let none = op(&host, "/queueMessage?app=notes&id=1");
    assert_eq!(none.status, 404, "{}", none.body);
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
    apps(&[sample_with("admin", config), post(TWO), sample("notes")])
}

/// A page of the admin app.
fn page(host: &Host, path: &str) -> Answer {
    let answer = send_raw(
        host.addr,
        format!(
            "GET {path} HTTP/1.1\r\nHost: {UI_HOST}\r\nConnection: close\r\n\
             Authorization: {UI_LOGIN}\r\n\r\n"
        )
        .as_bytes(),
    );
    assert_eq!(answer.status, 200, "{}", answer.body);
    answer
}

/// Disables an app through the admin app's own form, which is how a test
/// stops the delivery loop from taking what it is about to look at.
fn disable_by_form(host: &Host, app: &str) {
    let posted = send_raw(
        host.addr,
        format!(
            "POST /apps/{app}/disable HTTP/1.1\r\nHost: {UI_HOST}\r\nConnection: close\r\n\
             Authorization: {UI_LOGIN}\r\nOrigin: https://{UI_HOST}\r\n\
             Sec-Fetch-Site: same-origin\r\nContent-Length: 0\r\n\r\n"
        )
        .as_bytes(),
    );
    assert_eq!(posted.status, 303, "{}", posted.body);
}

#[test]
fn the_page_shows_each_queue_what_is_waiting_and_the_dead_letters() {
    let apps = ui_apps();
    let host = start(&apps, 2);
    disable_by_form(&host, "post");
    put(&apps, "post", "mail", "waiting body");
    let buried = put(&apps, "post", "mail", "buried body");
    bury(&apps, "post", buried, "500 gave up");

    let said = page(&host, "/apps/post/queues").body;
    // Every declared queue, the empty one included, with its settings.
    assert!(said.contains("<code>mail</code>"), "{said}");
    assert!(said.contains("<code>spare</code>"), "{said}");
    assert!(said.contains("2 attempts, retry 3600s"), "{said}");
    assert!(said.contains("lease 120s"), "{said}");
    // What is waiting, and what it is: due, never tried.
    assert!(said.contains("<code>waiting body</code>"), "{said}");
    assert!(said.contains(">due<"), "{said}");
    // And the dead letter with why, which is the reason to open the page.
    assert!(said.contains("<code>buried body</code>"), "{said}");
    assert!(said.contains("500 gave up"), "{said}");
    // The page says what it will not do, and where reviving is done.
    assert!(said.contains("Revive a dead letter"), "{said}");
    assert!(
        said.contains("minicloud queues post --revive &lt;id&gt;"),
        "{said}"
    );
    // The window took nothing: the message is still there to be delivered.
    assert!(
        waiting_ids(&apps).contains(&"waiting body".to_string()),
        "the message is still on the queue"
    );

    // The app's tabs are the way here, and the dead letter is said on the
    // Overview before anybody goes looking for it.
    let app = page(&host, "/apps/post").body;
    assert!(
        app.contains("<a href=\"/apps/post/queues\">Queues</a>"),
        "{app}"
    );
    assert!(app.contains("dead letter"), "{app}");
    // And an app not granted `queue` has no such tab.
    let notes = page(&host, "/apps/notes").body;
    assert!(!notes.contains("/apps/notes/queues"), "{notes}");
    let none = page(&host, "/apps/notes/queues").body;
    assert!(none.contains("Not granted <code>queue</code>."), "{none}");
    assert!(!none.contains("<th>queue</th>"), "{none}");
}

/// The bodies still on `post`'s queue, read straight from the store: the
/// page must not have changed what is there.
fn waiting_ids(apps: &Apps) -> Vec<String> {
    minicloud::queue::peek_waiting(&apps.data.join("post/queue.sqlite3"), now(), 100)
        .unwrap()
        .into_iter()
        .map(|message| message.body)
        .collect()
}

/// A message's body is whatever the outside sent — a webhook's payload, a
/// form somebody filled in — and the reason a delivery failed is whatever
/// the consumer answered. Both go into a page, so both are escaped by the
/// `html"…"` hole they go in, or this window would be the way into the admin
/// app.
#[test]
fn the_page_escapes_a_body_and_a_reason() {
    let apps = ui_apps();
    let host = start(&apps, 2);
    disable_by_form(&host, "post");
    let nasty = "<img src=y>&\"'";
    put(&apps, "post", "mail", nasty);
    let buried = put(&apps, "post", "mail", "fine");
    bury(&apps, "post", buried, &format!("500 {nasty}"));

    let said = page(&host, "/apps/post/queues").body;
    assert!(!said.contains("<img src=y>"), "{said}");
    assert_eq!(
        said.matches("&lt;img src=y&gt;&amp;&quot;&#39;").count(),
        2,
        "the body and the reason: {said}"
    );
}

/// The id of a message is the way to the whole of its body, as a key is in
/// the store's page: a listing cuts, and a queue's body is a payload.
#[test]
fn the_page_opens_one_message_whole_from_its_id() {
    let apps = ui_apps();
    let host = start(&apps, 2);
    disable_by_form(&host, "post");
    let long = format!("{}<img src=y>", "p".repeat(2000));
    let id = put(&apps, "post", "mail", &long);
    let buried = put(&apps, "post", "mail", "short");
    bury(&apps, "post", buried, "500 gave up");

    // The listing links to it; the page shows the body the listing cut.
    let listing = page(&host, "/apps/post/queues").body;
    assert!(
        listing.contains(&format!("/apps/post/queues?message={id}\"")),
        "{listing}"
    );
    let said = page(&host, &format!("/apps/post/queues?message={id}")).body;
    assert!(said.contains(&"p".repeat(2000)), "{said}");
    // Whole, and still escaped: it is a payload, not markup.
    assert!(!said.contains("<img src=y>"), "{said}");
    assert!(said.contains("&lt;img src=y&gt;"), "{said}");
    assert!(said.contains("<th>next due</th>"), "{said}");

    // A dead letter's page says when it was buried and why, and not a due
    // time it no longer has.
    let dead = page(&host, &format!("/apps/post/queues?message={buried}")).body;
    assert!(dead.contains("<th>buried</th>"), "{dead}");
    assert!(dead.contains("500 gave up"), "{dead}");
    assert!(!dead.contains("<th>next due</th>"), "{dead}");

    // A link to a message that has since gone says so and keeps the way
    // back.
    let gone = page(&host, "/apps/post/queues?message=9999").body;
    assert!(gone.contains("Not read."), "{gone}");
    assert!(gone.contains("may have been delivered since"), "{gone}");
    assert!(gone.contains("Back to queues"), "{gone}");
}

/// A body longer than the column is cut where the store's page cuts a
/// value, and says that it was.
#[test]
fn the_page_cuts_a_long_body_and_says_so() {
    let apps = ui_apps();
    let host = start(&apps, 2);
    disable_by_form(&host, "post");
    put(&apps, "post", "mail", &"x".repeat(400));

    let said = page(&host, "/apps/post/queues").body;
    assert!(said.contains(&"x".repeat(140)), "{said}");
    assert!(!said.contains(&"x".repeat(141)), "{said}");
    assert!(said.contains("…"), "{said}");
}

/// A listing cut at the window's limit says so, with the whole count beside
/// it: 200 shown as though they were all of 205 is the one wrong answer.
#[test]
fn the_page_says_when_it_is_showing_the_first_of_more() {
    let apps = ui_apps();
    let host = start(&apps, 2);
    disable_by_form(&host, "post");
    let more = minicloud::queue::PEEK_MESSAGES + 5;
    for n in 0..more {
        put(&apps, "post", "mail", &format!("message {n}"));
    }
    let said = page(&host, "/apps/post/queues").body;
    assert!(said.contains("The first 200"), "{said}");
    assert!(said.contains("minicloud queues"), "{said}");
    // The count in the table is whole, which is what makes the note useful.
    assert!(said.contains(&format!(">{more}</td>")), "{said}");
}
