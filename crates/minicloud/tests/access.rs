//! Cloudflare Access (issue #23): `auth.identity` verifying the Access token
//! for `apps/admin` and `examples/webhooks`, against a JWKS this test serves
//! itself, with RSA keys it generates itself.
//!
//! The mock JWKS is a plain TCP listener that answers every request with
//! whatever keys the test has put up (or 503), and counts the fetches. A key
//! fetch is rate-limited to one a second (`MIN_REFRESH_GAP`), so the tests
//! that rotate keys wait that long — it is the rate limit being honoured,
//! not a timing assertion.

mod common;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use common::*;
use jsonwebtoken::{EncodingKey, Header};
use minicloud::Host;
use rsa::pkcs1::EncodeRsaPrivateKey;
use rsa::traits::PublicKeyParts;
use serde_json::json;

const ADMIN_AUD: &str = "admin-aud-0123456789abcdef";
const MAIN_AUD: &str = "main-aud-fedcba9876543210";
const OWNER: &str = "owner@example.com";
const UI_HOST: &str = "covtools-admin.ramda.io";
const UI_SECRET: &str = "ui-secret";
const LAB_SECRET: &str = "lab-secret";

// ------------------------------------------------------------- the keys

/// An RSA key the test signs with, and its public half as a JWK.
struct Key {
    kid: String,
    encoding: EncodingKey,
    jwk: serde_json::Value,
}

fn generate(kid: &str) -> Key {
    let private = rsa::RsaPrivateKey::new(&mut rand::thread_rng(), 2048).unwrap();
    let b64 = |bytes: Vec<u8>| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let jwk = json!({
        "kid": kid,
        "kty": "RSA",
        "alg": "RS256",
        "use": "sig",
        "n": b64(private.n().to_bytes_be()),
        "e": b64(private.e().to_bytes_be()),
    });
    let pem = private.to_pkcs1_pem(rsa::pkcs1::LineEnding::LF).unwrap();
    Key {
        kid: kid.to_string(),
        encoding: EncodingKey::from_rsa_pem(pem.as_bytes()).unwrap(),
        jwk,
    }
}

/// Three keys, made once: generating one takes a while.
fn keys() -> &'static [Key; 3] {
    static KEYS: OnceLock<[Key; 3]> = OnceLock::new();
    KEYS.get_or_init(|| [generate("key-a"), generate("key-b"), generate("key-a")])
}

/// The key the JWKS serves first.
fn key_a() -> &'static Key {
    &keys()[0]
}
/// The key the JWKS rotates to.
fn key_b() -> &'static Key {
    &keys()[1]
}
/// A key nobody published, under `key-a`'s kid: a forged signature.
fn forger() -> &'static Key {
    &keys()[2]
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// A token as Cloudflare Access issues it, signed by `key`, with `claims`
/// changed by `edit`.
fn token(key: &Key, issuer: &str, aud: &str, edit: impl FnOnce(&mut serde_json::Value)) -> String {
    let now = now();
    let mut claims = json!({
        "aud": [aud],
        "email": OWNER,
        "exp": now + 3600,
        "iat": now,
        "nbf": now,
        "iss": issuer,
        "type": "app",
        "identity_nonce": "nonce",
        "sub": "0000-user",
        "country": "JP",
    });
    edit(&mut claims);
    let mut header = Header::new(jsonwebtoken::Algorithm::RS256);
    header.kid = Some(key.kid.clone());
    jsonwebtoken::encode(&header, &claims, &key.encoding).unwrap()
}

// ------------------------------------------------------- the mock JWKS

/// A JWKS at `http://<addr>/cdn-cgi/access/certs`: the keys in `served`, or
/// 503 when it is `None`.
struct Jwks {
    addr: SocketAddr,
    served: Arc<Mutex<Option<Vec<serde_json::Value>>>>,
    fetches: Arc<AtomicUsize>,
}

impl Jwks {
    fn start(keys: &[&Key]) -> Jwks {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let served = Arc::new(Mutex::new(Some(
            keys.iter().map(|key| key.jwk.clone()).collect(),
        )));
        let fetches = Arc::new(AtomicUsize::new(0));
        let (them, count) = (Arc::clone(&served), Arc::clone(&fetches));
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut request = Vec::new();
                let mut buffer = [0u8; 1024];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut buffer) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => request.extend_from_slice(&buffer[..n]),
                    }
                }
                let line = String::from_utf8_lossy(&request);
                let right = line.starts_with("GET /cdn-cgi/access/certs ");
                count.fetch_add(1, Ordering::SeqCst);
                let reply = match (right, them.lock().unwrap().clone()) {
                    (true, Some(keys)) => {
                        let body = json!({ "keys": keys, "public_cert": {}, "public_certs": [] })
                            .to_string();
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                    }
                    _ => "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\
                          Connection: close\r\n\r\n"
                        .to_string(),
                };
                let _ = stream.write_all(reply.as_bytes());
            }
        });
        Jwks {
            addr,
            served,
            fetches,
        }
    }

    /// The team "domain" the apps are configured with: the mock's URL.
    fn team(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn serve(&self, keys: Option<&[&Key]>) {
        *self.served.lock().unwrap() =
            keys.map(|keys| keys.iter().map(|key| key.jwk.clone()).collect());
    }

    fn fetches(&self) -> usize {
        self.fetches.load(Ordering::SeqCst)
    }
}

/// Past the key fetch rate limit, so the next unknown key fetches again.
fn past_the_rate_limit() {
    std::thread::sleep(Duration::from_millis(1100));
}

// ------------------------------------------------------------ the apps

/// The shipped `app.toml` of `app` with its deployment facts given
/// literally: the team is `team`, the AUD `aud`, the allowed emails
/// `emails`, the fallback `fallback`.
fn config(app: &str, team: &str, aud: &str, emails: &str, fallback: &str) -> String {
    let shipped = std::fs::read_to_string(sample_dir(app).join("app.toml")).unwrap();
    let (aud_env, secret_env, secret) = match app {
        "admin" => ("COVTOOLS_ADMIN_ACCESS_AUD", "ADMIN_UI_TOKEN", UI_SECRET),
        _ => ("COVTOOLS_ACCESS_AUD", "WEBHOOKS_ADMIN_TOKEN", LAB_SECRET),
    };
    let mut config = shipped;
    for (from, to) in [
        ("{ env = \"ACCESS_TEAM_DOMAIN\" }".to_string(), team),
        (format!("{{ env = \"{aud_env}\" }}"), aud),
        ("{ env = \"ACCESS_ALLOWED_EMAILS\" }".to_string(), emails),
        (format!("{{ env = \"{secret_env}\" }}"), secret),
    ] {
        assert!(
            config.contains(&from),
            "the shipped {app}/app.toml changed shape"
        );
        config = config.replace(&from, &format!("{{ value = \"{to}\" }}"));
    }
    assert!(config.contains("fallback = \"none\""));
    config = config.replace("fallback = \"none\"", &format!("fallback = \"{fallback}\""));
    if app == "webhooks" {
        let start = config.find("allow = [").unwrap();
        let end = start + config[start..].find(']').unwrap() + 1;
        config.replace_range(start..end, "allow = [\"http://127.0.0.1:*\"]");
    }
    config
}

fn leak(text: String) -> &'static str {
    Box::leak(text.into_boxed_str())
}

/// The admin app, the webhook lab and hello, behind Access at `jwks`.
fn deployed(jwks: &Jwks, fallback: &str) -> (Apps, Host) {
    let emails = format!("{OWNER}, Second@Example.com");
    let apps = apps(&[
        sample_with(
            "admin",
            leak(config("admin", &jwks.team(), ADMIN_AUD, &emails, fallback)),
        ),
        sample_with(
            "webhooks",
            leak(config(
                "webhooks",
                &jwks.team(),
                MAIN_AUD,
                &emails,
                fallback,
            )),
        ),
        sample("hello"),
    ]);
    let mut options = options(&apps, 2);
    options.forwarding.public_origin = Some("https://covtools.ramda.io".parse().unwrap());
    let host = Host::start(options).unwrap();
    (apps, host)
}

/// A request to the admin app with `headers`.
fn ui(host: &Host, method: &str, path: &str, headers: &[String]) -> Answer {
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: {UI_HOST}\r\nConnection: close\r\n");
    for header in headers {
        head.push_str(header);
        head.push_str("\r\n");
    }
    head.push_str("Content-Length: 0\r\n\r\n");
    send_raw(host.addr, head.as_bytes())
}

/// A request to the webhook lab with `headers`.
fn lab(host: &Host, method: &str, path: &str, headers: &[String], body: &str) -> Answer {
    let mut head =
        format!("{method} {path} HTTP/1.1\r\nHost: covtools.ramda.io\r\nConnection: close\r\n");
    for header in headers {
        head.push_str(header);
        head.push_str("\r\n");
    }
    head.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
    send_raw(host.addr, head.as_bytes())
}

fn assertion(token: &str) -> String {
    format!("Cf-Access-Jwt-Assertion: {token}")
}

/// A good token for the admin app.
fn admin_token(jwks: &Jwks) -> String {
    token(key_a(), &jwks.team(), ADMIN_AUD, |_| {})
}

/// What a browser on the admin pages sends with a form.
fn same_site() -> Vec<String> {
    vec![
        format!("Origin: https://{UI_HOST}"),
        "Sec-Fetch-Site: same-origin".to_string(),
    ]
}

/// A refusal behind Access: 403, and no prompt.
fn assert_refused(answer: &Answer, what: &str) {
    assert_eq!(answer.status, 403, "{what}: {}", answer.body);
    assert_eq!(answer.header("www-authenticate"), None, "{what}");
}

// ------------------------------------------------------------ the tests

#[test]
fn a_valid_token_gets_in_and_the_history_records_its_email() {
    let jwks = Jwks::start(&[key_a()]);
    let (_apps, host) = deployed(&jwks, "none");
    let good = admin_token(&jwks);
    let page = ui(&host, "GET", "/", &[assertion(&good)]);
    assert_eq!(page.status, 200, "{}", page.body);
    assert!(page.body.contains(">hello</a>"), "{}", page.body);
    // Every app's log is behind the same identity as the rest (issue #108):
    // it is every app's lines, so it is the page that is worth the most to
    // somebody who did not get in.
    assert_eq!(ui(&host, "GET", "/logs", &[assertion(&good)]).status, 200);
    assert_refused(&ui(&host, "GET", "/logs", &[]), "no token");
    // The browser's cookie does as well as the header.
    let cookie = format!("Cookie: theme=dark; CF_Authorization={good}");
    assert_eq!(ui(&host, "GET", "/history", &[cookie]).status, 200);
    // A change is recorded as the verified email — not as whatever
    // `Cf-Access-Authenticated-User-Email` says, which anyone can send.
    let mut headers = same_site();
    headers.push(assertion(&good));
    headers.push("Cf-Access-Authenticated-User-Email: mallory@example.com".to_string());
    let disabled = ui(&host, "POST", "/apps/hello/disable", &headers);
    assert_eq!(disabled.status, 303, "{}", disabled.body);
    assert_eq!(get(host.addr, "/hello/").status, 503);
    let history = ui(&host, "GET", "/history", &[assertion(&good)]).body;
    assert!(
        history.contains(&format!("admin app: {OWNER}")),
        "{history}"
    );
    assert!(!history.contains("mallory"), "{history}");
    // One fetch of the keys served all of it.
    assert_eq!(jwks.fetches(), 1);
    // An allowed email in another case is the same user.
    let second = token(key_a(), &jwks.team(), ADMIN_AUD, |claims| {
        claims["email"] = json!("second@example.com");
    });
    assert_eq!(ui(&host, "GET", "/", &[assertion(&second)]).status, 200);
}

#[test]
fn a_bad_token_or_none_is_refused_without_a_prompt() {
    let jwks = Jwks::start(&[key_a()]);
    let (_apps, host) = deployed(&jwks, "none");
    let team = jwks.team();
    let at = |edit: &dyn Fn(&mut serde_json::Value)| {
        token(key_a(), &team, ADMIN_AUD, |claims| edit(claims))
    };
    let cases: Vec<(&str, Option<String>)> = vec![
        ("no token", None),
        ("not a JWT", Some("not.a.jwt".to_string())),
        (
            "a forged signature",
            Some(token(forger(), &team, ADMIN_AUD, |_| {})),
        ),
        (
            "the main hostname's aud",
            Some(token(key_a(), &team, MAIN_AUD, |_| {})),
        ),
        (
            "another team",
            Some(token(
                key_a(),
                "https://evil.cloudflareaccess.com",
                ADMIN_AUD,
                |_| {},
            )),
        ),
        (
            "expired",
            Some(at(&|claims| claims["exp"] = json!(now() - 3600))),
        ),
        (
            "not valid yet",
            Some(at(&|claims| claims["nbf"] = json!(now() + 3600))),
        ),
        (
            "no email (a service token)",
            Some(at(&|claims| {
                claims.as_object_mut().unwrap().remove("email");
                claims["common_name"] = json!("ci.access");
            })),
        ),
        (
            "an email not allowed",
            Some(at(&|claims| {
                claims["email"] = json!("stranger@example.com")
            })),
        ),
        (
            "no exp",
            Some(at(&|claims| {
                claims.as_object_mut().unwrap().remove("exp");
            })),
        ),
    ];
    for (what, token) in &cases {
        let headers: Vec<String> = token.iter().map(|token| assertion(token)).collect();
        let page = ui(&host, "GET", "/", &headers);
        assert_refused(&page, what);
        assert!(
            page.body.contains("reached through Cloudflare Access"),
            "{what}: {}",
            page.body
        );
        let mut form = same_site();
        form.extend(headers);
        assert_refused(&ui(&host, "POST", "/apps/hello/disable", &form), what);
    }
    assert_eq!(get(host.addr, "/hello/").status, 200, "nothing changed");
    // The reasons say what was wrong.
    let expired = ui(
        &host,
        "GET",
        "/",
        &[assertion(&cases[5].1.clone().unwrap())],
    );
    assert!(expired.body.contains("expired"), "{}", expired.body);
    let aud = ui(
        &host,
        "GET",
        "/",
        &[assertion(&cases[3].1.clone().unwrap())],
    );
    assert!(aud.body.contains("aud"), "{}", aud.body);
    // Straight to the host's port with the secret, as before: refused, the
    // secret being no way in while Access is on (`fallback = "none"`).
    for secret in [
        format!("Authorization: Bearer {UI_SECRET}"),
        format!(
            "Authorization: Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("x:{UI_SECRET}"))
        ),
    ] {
        assert_refused(&ui(&host, "GET", "/", &[secret]), "the secret alone");
    }
}

#[test]
fn a_new_signing_key_is_fetched_when_a_token_names_it() {
    let jwks = Jwks::start(&[key_a()]);
    let (_apps, host) = deployed(&jwks, "none");
    let team = jwks.team();
    assert_eq!(
        ui(&host, "GET", "/", &[assertion(&admin_token(&jwks))]).status,
        200
    );
    assert_eq!(jwks.fetches(), 1);
    // Cloudflare rotates: the JWKS now has key b as well, and a token
    // signed by it fetches the keys again.
    jwks.serve(Some(&[key_a(), key_b()]));
    past_the_rate_limit();
    let rotated = token(key_b(), &team, ADMIN_AUD, |_| {});
    let page = ui(&host, "GET", "/", &[assertion(&rotated)]);
    assert_eq!(page.status, 200, "{}", page.body);
    assert_eq!(jwks.fetches(), 2);
    // Held now: no more fetches, for either key.
    assert_eq!(ui(&host, "GET", "/", &[assertion(&rotated)]).status, 200);
    assert_eq!(
        ui(&host, "GET", "/", &[assertion(&admin_token(&jwks))]).status,
        200
    );
    assert_eq!(jwks.fetches(), 2);
    // A kid nobody publishes is refused, and asking again at once does not
    // fetch again: one fetch a second at most, whatever tokens say.
    let mut unknown = Header::new(jsonwebtoken::Algorithm::RS256);
    unknown.kid = Some("key-z".to_string());
    let claims = json!({ "aud": [ADMIN_AUD], "email": OWNER, "exp": now() + 60, "iss": team });
    let stranger = jsonwebtoken::encode(&unknown, &claims, &key_b().encoding).unwrap();
    past_the_rate_limit();
    let first = ui(&host, "GET", "/", &[assertion(&stranger)]);
    assert_refused(&first, "an unknown kid");
    assert!(
        first.body.contains("not among the team's"),
        "{}",
        first.body
    );
    assert_eq!(jwks.fetches(), 3);
    for _ in 0..5 {
        assert_refused(&ui(&host, "GET", "/", &[assertion(&stranger)]), "again");
    }
    assert_eq!(jwks.fetches(), 3, "rate-limited");
}

#[test]
fn with_the_keys_unreachable_every_token_is_refused() {
    let jwks = Jwks::start(&[key_a()]);
    jwks.serve(None);
    let (_apps, host) = deployed(&jwks, "none");
    let good = admin_token(&jwks);
    let page = ui(&host, "GET", "/", &[assertion(&good)]);
    assert_refused(&page, "keys unreachable");
    assert!(page.body.contains("could not be fetched"), "{}", page.body);
    // Logged, for the operator.
    let logs = app_log(&host);
    assert!(
        logs.contains("cannot fetch Cloudflare Access's signing keys"),
        "{logs}"
    );
    // Back up: the next token past the rate limit gets in.
    jwks.serve(Some(&[key_a()]));
    past_the_rate_limit();
    assert_eq!(ui(&host, "GET", "/", &[assertion(&good)]).status, 200);
}

/// The admin app's recent log lines.
fn app_log(host: &Host) -> String {
    get(host.addr, "/_host/apps/admin/logs?n=50").body
}

#[test]
fn the_token_is_a_way_in_only_where_the_app_says_so() {
    // `fallback = "token"`: the secret gets in too, recorded as a token.
    let jwks = Jwks::start(&[key_a()]);
    let (_apps, host) = deployed(&jwks, "token");
    let bearer = format!("Authorization: Bearer {UI_SECRET}");
    assert_eq!(
        ui(&host, "GET", "/", std::slice::from_ref(&bearer)).status,
        200
    );
    let mut headers = same_site();
    headers.push(bearer.clone());
    assert_eq!(
        ui(&host, "POST", "/apps/hello/disable", &headers).status,
        303
    );
    let history = ui(&host, "GET", "/history", &[bearer]).body;
    assert!(history.contains("admin app: token"), "{history}");
    // A wrong secret is still nothing, and still no prompt: Access is on.
    assert_refused(
        &ui(
            &host,
            "GET",
            "/",
            &["Authorization: Bearer wrong".to_string()],
        ),
        "a wrong secret",
    );
    // And a token still gets in as itself.
    assert_eq!(
        ui(&host, "GET", "/", &[assertion(&admin_token(&jwks))]).status,
        200
    );
}

#[test]
fn with_access_off_the_secret_and_its_prompt_are_as_before() {
    // No team: a local run. The admin app prompts for its secret.
    let config = config("admin", "", "", "", "none");
    let apps = apps(&[sample_with("admin", leak(config)), sample("hello")]);
    let host = start(&apps, 1);
    let refused = ui(&host, "GET", "/", &[]);
    assert_eq!(refused.status, 401);
    assert!(refused
        .header("www-authenticate")
        .unwrap()
        .starts_with("Basic"));
    let bearer = format!("Authorization: Bearer {UI_SECRET}");
    assert_eq!(ui(&host, "GET", "/", &[bearer]).status, 200);
    // A token means nothing to an app with Access off.
    let anything = token(key_a(), "https://x.cloudflareaccess.com", ADMIN_AUD, |_| {});
    assert_eq!(ui(&host, "GET", "/", &[assertion(&anything)]).status, 401);
    // The host said, when it loaded the app, that Access is off.
    assert!(
        app_log(&host).contains("Cloudflare Access is off"),
        "{}",
        app_log(&host)
    );
}

#[test]
fn the_webhook_labs_pages_need_a_token_and_its_receive_urls_do_not() {
    let jwks = Jwks::start(&[key_a()]);
    let (_apps, host) = deployed(&jwks, "none");
    let team = jwks.team();
    let good = assertion(&token(key_a(), &team, MAIN_AUD, |_| {}));
    // The admin app's token is for another Access application.
    let other = assertion(&admin_token(&jwks));
    for headers in [
        vec![],
        vec![other],
        vec![format!("Authorization: Bearer {LAB_SECRET}")],
    ] {
        let page = lab(&host, "GET", "/webhooks/admin", &headers, "");
        assert_refused(&page, &format!("{headers:?}"));
    }
    assert_eq!(
        lab(
            &host,
            "GET",
            "/webhooks/admin",
            std::slice::from_ref(&good),
            ""
        )
        .status,
        200
    );
    let made = lab(
        &host,
        "POST",
        "/webhooks/admin/endpoints",
        &[good.clone(), "Accept: application/json".to_string()],
        "name=open",
    );
    assert_eq!(made.status, 201, "{}", made.body);
    let made: serde_json::Value = serde_json::from_str(&made.body).unwrap();
    let id = made["id"].as_str().unwrap();
    // A sender has no token and needs none.
    let fetches = jwks.fetches();
    let received = lab(&host, "POST", &format!("/webhooks/in/{id}"), &[], "hi");
    assert_eq!(received.status, 200, "{}", received.body);
    let bogus = lab(
        &host,
        "POST",
        &format!("/webhooks/in/{id}"),
        &["Cf-Access-Jwt-Assertion: garbage".to_string()],
        "again",
    );
    assert_eq!(bogus.status, 200, "{}", bogus.body);
    assert_eq!(jwks.fetches(), fetches, "receiving fetched no keys");
    let history = lab(
        &host,
        "GET",
        &format!("/webhooks/admin/e/{id}?format=json"),
        &[good],
        "",
    );
    assert_eq!(history.status, 200, "{}", history.body);
    let history: serde_json::Value = serde_json::from_str(&history.body).unwrap();
    assert_eq!(history["events"].as_array().unwrap().len(), 2, "{history}");
}
