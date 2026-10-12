//! ACME: the host gets its own certificate, and keeps it.
//!
//! `--tls-cert` and `--tls-key` ([`crate::tls`]) are a pair something else
//! renews. `--acme-domain` is the host doing it: an account with the
//! certificate authority, an order for the names it serves, an HTTP-01
//! challenge answered on the plain port, and a renewal clock.
//!
//! ```console
//! $ minicloud serve --apps apps --addr 0.0.0.0:443 \
//!     --acme-domain tools.example --acme-email me@example \
//!     --acme-http 0.0.0.0:80
//! ```
//!
//! # What is kept, and where
//!
//! `<data>/_host/acme/`: `account.json` (the account's key, which is the
//! host's name at the authority), `cert.pem` and `key.pem` (what it is
//! serving). A start that finds a certificate with more than
//! [`RENEW_WITHIN`] left **serves it and orders nothing** — a host that
//! ordered on every start would be rate-limited by any authority worth
//! using, and the certificate on disk is the one thing that must survive a
//! restart.
//!
//! # The challenge
//!
//! HTTP-01: the authority asks for `http://<domain>/.well-known/acme-
//! challenge/<token>` over **port 80, in the clear**, which is the one
//! thing a host that has no certificate yet can answer. `--acme-http` is
//! that listener; it answers the challenges it has been given and redirects
//! everything else to `https://`, so it is also the "http to https" every
//! deployment wants anyway.
//!
//! # What is not tested
//!
//! The exchange with an authority is not: it needs an authority, a public
//! name and port 80, none of which a test has. What is tested is everything
//! this host owns — the challenge listener, the renewal clock, what is
//! written and read back, and that a certificate already on disk is served
//! without an order. The exchange itself is `instant-acme`'s, and the first
//! run of a deployment should use `--acme-staging` to find out whether the
//! name and the port are right before asking the real authority.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How long before a certificate runs out the host renews it.
///
/// Thirty days: a certificate that is good for ninety is renewed with a
/// third of its life left, so a month of failures is a month of warnings
/// rather than an outage.
pub const RENEW_WITHIN: Duration = Duration::from_secs(30 * 24 * 3600);

/// How often the host looks at how long its certificate has left.
pub const CHECK_EVERY: Duration = Duration::from_secs(12 * 3600);

/// What the host asks an authority for.
#[derive(Clone, Debug)]
pub struct Order {
    /// The names the certificate is for; the first is its subject.
    pub domains: Vec<String>,
    /// The address to tell the authority about, for its expiry warnings.
    pub email: Option<String>,
    /// Let's Encrypt's staging authority, whose certificates are not
    /// trusted and whose rate limits are kind. The first run of a
    /// deployment belongs here.
    pub staging: bool,
    /// Where `account.json`, `cert.pem` and `key.pem` are kept.
    pub dir: PathBuf,
}

impl Order {
    pub fn cert(&self) -> PathBuf {
        self.dir.join("cert.pem")
    }

    pub fn key(&self) -> PathBuf {
        self.dir.join("key.pem")
    }

    fn account(&self) -> PathBuf {
        self.dir.join("account.json")
    }

    /// The pair this order writes, for the listener to serve.
    pub fn pair(&self) -> crate::tls::Pair {
        crate::tls::Pair {
            cert: self.cert(),
            key: self.key(),
        }
    }
}

/// The tokens the authority is about to ask for, by the path it will ask
/// at.
///
/// A challenge is answered for as long as the order is being made and
/// forgotten after, so a token that was never asked for does not stay an
/// answer for ever.
#[derive(Default)]
pub struct Challenges {
    answers: Mutex<HashMap<String, String>>,
}

/// Where the authority asks for a challenge.
pub const CHALLENGE_PREFIX: &str = "/.well-known/acme-challenge/";

impl Challenges {
    /// Answers `token` with `key_authorization` until it is taken back.
    pub fn offer(&self, token: &str, key_authorization: &str) {
        self.answers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(token.to_string(), key_authorization.to_string());
    }

    /// Forgets `token`.
    pub fn withdraw(&self, token: &str) {
        self.answers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(token);
    }

    /// What to answer a request for `path`, if it is a challenge this host
    /// is offering.
    pub fn answer(&self, path: &str) -> Option<String> {
        let token = path.strip_prefix(CHALLENGE_PREFIX)?;
        self.answers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(token)
            .cloned()
    }

    pub fn len(&self) -> usize {
        self.answers.lock().unwrap_or_else(|p| p.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// How long the certificate at `path` has left, if it reads.
///
/// The parsing is deliberately small: the one field this host needs is
/// `notAfter`, and a certificate is only ever one this host wrote.
pub fn left(path: &Path) -> Option<Duration> {
    let pem = std::fs::read(path).ok()?;
    let certs: Vec<_> = rustls_pemfile::certs(&mut pem.as_slice())
        .collect::<Result<_, _>>()
        .ok()?;
    let leaf = certs.first()?;
    let not_after = not_after(leaf)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    (not_after > now).then(|| Duration::from_secs((not_after - now) as u64))
}

/// `notAfter` of a DER certificate, as seconds since the epoch.
///
/// A certificate is a `Certificate` holding a `TBSCertificate` whose fifth
/// field is a `Validity` of two times. Walking that far by hand is a
/// hundred lines less than a parser crate, and the only thing that could
/// change under it is the shape of X.509 itself.
fn not_after(der: &[u8]) -> Option<i64> {
    let (certificate, _) = der_field(der, 0x30)?; // Certificate
    let (tbs, _) = der_field(certificate, 0x30)?; // TBSCertificate
                                                  // `validity` is the one field of the certificate that is a sequence of
                                                  // two times, which is a surer way to find it than counting fields that
                                                  // an optional version shifts.
    let mut rest = tbs;
    while !rest.is_empty() {
        let tag = *rest.first()?;
        let (body, after) = der_field(rest, tag)?;
        if tag == 0x30 && matches!(body.first(), Some(0x17 | 0x18)) {
            let first = *body.first()?;
            let (_, after_first) = der_field(body, first)?;
            let second = *after_first.first()?;
            let (not_after, _) = der_field(after_first, second)?;
            return der_time(not_after);
        }
        rest = after;
    }
    None
}

/// The body of the next DER field and what follows it.
fn der_field(bytes: &[u8], expect: u8) -> Option<(&[u8], &[u8])> {
    let tag = *bytes.first()?;
    if tag != expect {
        return None;
    }
    let first = *bytes.get(1)? as usize;
    let (length, at) = if first < 0x80 {
        (first, 2)
    } else {
        let count = first & 0x7f;
        if count == 0 || count > 4 {
            return None;
        }
        let mut length = 0usize;
        for index in 0..count {
            length = (length << 8) | *bytes.get(2 + index)? as usize;
        }
        (length, 2 + count)
    };
    let body = bytes.get(at..at + length)?;
    Some((body, bytes.get(at + length..)?))
}

/// A `UTCTime` (`YYMMDDHHMMSSZ`) or `GeneralizedTime` (`YYYYMMDDHHMMSSZ`)
/// as seconds since the epoch.
fn der_time(bytes: &[u8]) -> Option<i64> {
    let text = std::str::from_utf8(bytes).ok()?;
    let digits = text.trim_end_matches('Z');
    let (year, rest) = match digits.len() {
        // UTCTime: a two-digit year, 50 and above being the last century.
        12 => {
            let year: i64 = digits.get(..2)?.parse().ok()?;
            (
                if year >= 50 { 1900 + year } else { 2000 + year },
                &digits[2..],
            )
        }
        14 => (digits.get(..4)?.parse().ok()?, &digits[4..]),
        _ => return None,
    };
    let number = |at: usize| -> Option<i64> { rest.get(at..at + 2)?.parse().ok() };
    let (month, day, hour, minute, second) =
        (number(0)?, number(2)?, number(4)?, number(6)?, number(8)?);
    Some(
        crate::cron::Minute {
            year,
            month: month as u32,
            day: day as u32,
            hour: hour as u32,
            minute: minute as u32,
            weekday: 0,
        }
        .to_unix()
            + second,
    )
}

/// Gets a certificate for `order` and writes it, answering challenges on
/// `challenges` while the authority asks.
///
/// This is the one part that talks to an authority, and the one part no
/// test here covers.
pub async fn obtain(order: &Order, challenges: &Challenges) -> Result<(), String> {
    use instant_acme::{
        Account, AuthorizationStatus, ChallengeType, Identifier, NewAccount, NewOrder, OrderStatus,
    };

    std::fs::create_dir_all(&order.dir)
        .map_err(|e| format!("cannot make the ACME directory: {e}"))?;
    let directory = if order.staging {
        instant_acme::LetsEncrypt::Staging.url()
    } else {
        instant_acme::LetsEncrypt::Production.url()
    };

    // The account is the host's name at the authority: kept, because a new
    // one every start is a new rate-limit allowance every start, which the
    // authority is right to refuse.
    let account = match std::fs::read(order.account()) {
        Ok(kept) => {
            let credentials = serde_json::from_slice(&kept)
                .map_err(|e| format!("the kept ACME account does not read: {e}"))?;
            Account::from_credentials(credentials)
                .await
                .map_err(|e| format!("the kept ACME account is not one: {e}"))?
        }
        Err(_) => {
            let contact: Vec<String> = order
                .email
                .iter()
                .map(|email| format!("mailto:{email}"))
                .collect();
            let contact: Vec<&str> = contact.iter().map(String::as_str).collect();
            let (account, credentials) = Account::create(
                &NewAccount {
                    contact: &contact,
                    terms_of_service_agreed: true,
                    only_return_existing: false,
                },
                directory,
                None,
            )
            .await
            .map_err(|e| format!("cannot make an ACME account: {e}"))?;
            let written = serde_json::to_vec_pretty(&credentials)
                .map_err(|e| format!("cannot write the ACME account: {e}"))?;
            write_private(&order.account(), &written)?;
            account
        }
    };

    let identifiers: Vec<Identifier> = order
        .domains
        .iter()
        .map(|domain| Identifier::Dns(domain.clone()))
        .collect();
    let mut placed = account
        .new_order(&NewOrder {
            identifiers: &identifiers,
        })
        .await
        .map_err(|e| format!("the ACME order was refused: {e}"))?;

    let authorizations = placed
        .authorizations()
        .await
        .map_err(|e| format!("cannot read the ACME authorizations: {e}"))?;
    let mut offered = Vec::new();
    for authorization in &authorizations {
        if authorization.status == AuthorizationStatus::Valid {
            continue;
        }
        let challenge = authorization
            .challenges
            .iter()
            .find(|challenge| challenge.r#type == ChallengeType::Http01)
            .ok_or_else(|| "the authority offered no HTTP-01 challenge".to_string())?;
        challenges.offer(
            &challenge.token,
            placed.key_authorization(challenge).as_str(),
        );
        offered.push(challenge.token.clone());
        placed
            .set_challenge_ready(&challenge.url)
            .await
            .map_err(|e| format!("cannot tell the authority the challenge is ready: {e}"))?;
    }

    // Waiting: the authority fetches the challenge and says so. The delay
    // doubles, because an authority that has not got to it yet is not
    // helped by being asked again at once.
    let mut wait = Duration::from_secs(1);
    let outcome = loop {
        let state = placed
            .refresh()
            .await
            .map_err(|e| format!("cannot read the ACME order: {e}"))?;
        match state.status {
            OrderStatus::Ready | OrderStatus::Valid | OrderStatus::Invalid => break state.status,
            _ if wait > Duration::from_secs(64) => {
                break OrderStatus::Invalid;
            }
            _ => {
                tokio::time::sleep(wait).await;
                wait *= 2;
            }
        }
    };
    for token in &offered {
        challenges.withdraw(token);
    }
    if outcome == OrderStatus::Invalid {
        return Err(
            "the authority did not validate the challenge; is the name pointed here, \
                    and is `--acme-http` reachable on port 80?"
                .to_string(),
        );
    }

    // The key is the host's and never leaves it: the authority signs a
    // request made from it.
    let mut params = rcgen::CertificateParams::new(order.domains.clone())
        .map_err(|e| format!("cannot make the signing request: {e}"))?;
    params.distinguished_name = rcgen::DistinguishedName::new();
    let key = rcgen::KeyPair::generate().map_err(|e| format!("cannot make a key: {e}"))?;
    let csr = params
        .serialize_request(&key)
        .map_err(|e| format!("cannot make the signing request: {e}"))?;
    placed
        .finalize(csr.der())
        .await
        .map_err(|e| format!("the authority refused the signing request: {e}"))?;

    let mut wait = Duration::from_secs(1);
    let chain = loop {
        match placed
            .certificate()
            .await
            .map_err(|e| format!("cannot fetch the certificate: {e}"))?
        {
            Some(chain) => break chain,
            None if wait > Duration::from_secs(64) => {
                return Err("the authority did not issue the certificate".to_string())
            }
            None => {
                tokio::time::sleep(wait).await;
                wait *= 2;
            }
        }
    };

    write_private(&order.key(), key.serialize_pem().as_bytes())?;
    std::fs::write(order.cert(), chain)
        .map_err(|e| format!("cannot write the certificate: {e}"))?;
    Ok(())
}

/// Writes a file only its owner may read, as the secret store does.
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    std::fs::write(path, bytes).map_err(|e| format!("cannot write `{}`: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// The plain listener: the challenges this host is offering, and a
/// redirect to `https://` for everything else.
///
/// It is HTTP/1.1 and deliberately small — it answers two things and has no
/// apps behind it — because it is the one door that is open before the host
/// has a certificate at all.
pub async fn serve_challenges(listener: tokio::net::TcpListener, challenges: Arc<Challenges>) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            tokio::time::sleep(Duration::from_millis(10)).await;
            continue;
        };
        let challenges = Arc::clone(&challenges);
        tokio::spawn(async move {
            let service = hyper::service::service_fn(move |request: hyper::Request<_>| {
                let challenges = Arc::clone(&challenges);
                async move {
                    let path = request.uri().path().to_string();
                    let answer = match challenges.answer(&path) {
                        // The key authorization, as it is: the authority
                        // compares the whole body.
                        Some(key) => hyper::Response::builder()
                            .status(200)
                            .header("content-type", "application/octet-stream")
                            .body(http_body_util::Full::new(bytes::Bytes::from(key))),
                        None => {
                            let host = request
                                .headers()
                                .get(hyper::header::HOST)
                                .and_then(|value| value.to_str().ok())
                                .unwrap_or_default()
                                .to_string();
                            let query = request
                                .uri()
                                .query()
                                .map(|query| format!("?{query}"))
                                .unwrap_or_default();
                            hyper::Response::builder()
                                .status(308)
                                .header("location", format!("https://{host}{path}{query}"))
                                .body(http_body_util::Full::new(bytes::Bytes::new()))
                        }
                    };
                    answer.or_else(|_| {
                        hyper::Response::builder()
                            .status(500)
                            .body(http_body_util::Full::new(bytes::Bytes::new()))
                    })
                }
            });
            let _ = hyper::server::conn::http1::Builder::new()
                .timer(hyper_util::rt::TokioTimer::new())
                .header_read_timeout(Duration::from_secs(10))
                .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                .await;
        });
    }
}

/// Gets a certificate if there is none or it is running out, and keeps
/// doing so for as long as the host runs.
pub async fn keep(
    order: Order,
    challenges: Arc<Challenges>,
    certificates: Arc<crate::tls::Certificates>,
) {
    loop {
        let left = left(&order.cert());
        let due = match left {
            Some(left) => left <= RENEW_WITHIN,
            None => true,
        };
        if due {
            match obtain(&order, &challenges).await {
                Ok(()) => match certificates.reload() {
                    Ok(()) => eprintln!(
                        "minicloud: got a certificate for {} from the {} authority",
                        order.domains.join(", "),
                        if order.staging { "staging" } else { "ACME" }
                    ),
                    Err(why) => eprintln!("minicloud: the new certificate does not read: {why}"),
                },
                // Not fatal: the certificate that is serving goes on
                // serving, and there are thirty days of these before it
                // runs out.
                Err(why) => eprintln!("minicloud: cannot get a certificate: {why}"),
            }
        }
        tokio::time::sleep(CHECK_EVERY).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_challenge_is_answered_at_its_path_and_only_until_it_is_withdrawn() {
        let challenges = Challenges::default();
        assert_eq!(challenges.answer("/.well-known/acme-challenge/abc"), None);
        challenges.offer("abc", "abc.the-key");
        assert_eq!(
            challenges
                .answer("/.well-known/acme-challenge/abc")
                .as_deref(),
            Some("abc.the-key")
        );
        // Only that path, and only that token.
        assert_eq!(challenges.answer("/abc"), None);
        assert_eq!(challenges.answer("/.well-known/acme-challenge/other"), None);
        challenges.withdraw("abc");
        assert_eq!(challenges.answer("/.well-known/acme-challenge/abc"), None);
        assert!(challenges.is_empty());
    }

    #[test]
    fn how_long_a_certificate_has_left_is_read_off_it() {
        let dir = std::env::temp_dir().join(format!("minicloud-acme-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cert.pem");

        // A certificate `rcgen` makes is good for a while: the host should
        // read a positive time off it, and not renew it.
        let mut params = rcgen::CertificateParams::new(vec!["tools.example".to_string()]).unwrap();
        params.not_after = rcgen::date_time_ymd(2099, 1, 1);
        let key = rcgen::KeyPair::generate().unwrap();
        let certified = params.clone().self_signed(&key).unwrap();
        std::fs::write(&path, certified.pem()).unwrap();
        let remaining = left(&path).expect("it reads");
        assert!(
            remaining > RENEW_WITHIN,
            "a certificate good until 2099 is not due: {remaining:?}"
        );

        // One that has run out reads as nothing left, which is what asks
        // for a new one.
        params.not_after = rcgen::date_time_ymd(2001, 1, 1);
        params.not_before = rcgen::date_time_ymd(2000, 1, 1);
        let expired = params.clone().self_signed(&key).unwrap();
        std::fs::write(&path, expired.pem()).unwrap();
        assert_eq!(left(&path), None);

        // And one that is not a certificate at all.
        std::fs::write(&path, "not a certificate\n").unwrap();
        assert_eq!(left(&path), None);
        assert_eq!(left(&dir.join("nothing.pem")), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
