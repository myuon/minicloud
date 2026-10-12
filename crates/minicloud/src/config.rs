//! `apps/<name>/app.toml`: what an app is granted and what bounds it.
//!
//! ```toml
//! entry = "hello.handle"          # optional; `<name>.handle` by default
//! grant = ["log", "timer"]        # capabilities; nothing by default
//!
//! [limits]
//! max_host_calls = 100            # per request
//! deadline = "5s"                 # per request, parked time included ("ms" or "s")
//! max_call_depth = 512            # per request
//! max_heap_words = 1048576        # per request, see `AppLimits::max_heap_words`
//! max_in_flight = 16              # runs started and not answered, this app
//! max_queued = 64                 # requests waiting to start, this app
//! max_request_bytes = 1048576     # request body
//! max_response_bytes = 1048576    # response body
//!
//! [assets]
//! max_bytes = 8388608             # the files of `assets/` together
//! cache_control = "no-cache"      # what every one of them is answered with
//!
//! [defer]                         # work that outlives a request, with `defer` granted
//! max_in_flight = 8               # deferred runs of this app that have not finished
//!
//! [rate]                          # how often one caller may reach the app
//! requests = 60                   # over `per`; without it there is no limit
//! per = "1m"
//! burst = 60                      # the most it may hold; `requests` by default
//! key = "address"                 # or "app", or "header:cf-connecting-ip"
//!
//! [sql]                           # the app's SQL store, with `sql` granted
//! max_bytes = 67108864            # the database
//! max_rows = 1000                 # rows one query may answer
//! max_result_bytes = 4194304      # the values of one answer, summed
//! max_statement_ms = 20           # how long one statement may run
//! max_statement_bytes = 65536     # how long its text may be
//!
//! [[triggers]]                   # the host runs the app itself, on a schedule
//! cron = "0 * * * *"             # UTC, to the minute; or @hourly, @daily, …
//! path = "/cron/hourly"          # the path the run is given; `/` by default
//! method = "POST"                # the method it is given
//!
//! [route]
//! hosts = ["admin.example"]       # reached by these hostnames only, not /<name>/
//!
//! [access]                        # Cloudflare Access, for `auth.identity`
//! team = { env = "ACCESS_TEAM_DOMAIN" }       # `<team>.cloudflareaccess.com`
//! aud = { env = "COVTOOLS_ACCESS_AUD" }       # the Access application's AUD tag(s)
//! emails = { env = "ACCESS_ALLOWED_EMAILS" }  # optional: only these
//! token = "admin"                 # a `[secrets]` name: the token way in
//! fallback = "none"               # or "token": the token too when Access is on
//! ```
//!
//! `[access]` is read by [`crate::access`]; its values are deployment
//! facts, so they come from the environment (or a file) like a secret's, and
//! an unset `team` or `aud` turns Access off for the app rather than
//! refusing it.
//!
//! What `app.toml` says can be changed at run time by the admin app
//! ([`AppOverride`], kept in the data directory by [`crate::overrides`]): the
//! grant, the fetch allowlist and the limits. An override is applied to the
//! file as written before anything is validated, so an overridden config is
//! held to every rule an `app.toml` is.
//!
//! Every key is optional and an unknown one is refused: a misspelt limit would
//! otherwise be a limit silently not applied. A refusal is the app's alone —
//! the host starts every other app.
//!
//! `limits.fuel` is a key that was removed: Cove's ADR 0091 took the fuel
//! allowance out of the runtime, and a request is bounded by `deadline`
//! instead. A file that still says it is refused with that reason where the
//! file is being put forward — `minicloud check`, `test`, `deploy`,
//! `update` — but an app already deployed with it is loaded at start with the
//! key ignored and a warning ([`RemovedKeys`]), so that upgrading the host
//! does not take down an app that has not been redeployed yet.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use cove_runtime::Limits;
use serde::{Deserialize, Serialize};

use crate::secrets::SecretStore;

/// The capability of the `admin` host module: listing the apps and changing
/// their configuration.
pub const ADMIN_CAPABILITY: &str = "admin";

/// The one app that may be granted [`ADMIN_CAPABILITY`]. Granting it to any
/// other app — in its `app.toml` or through an override — refuses that app.
pub const ADMIN_APP: &str = "admin";

/// The largest heap a run can have, in words.
///
/// `max_heap_words` is the capacity a run's heap is built with
/// (`OwnedVm::with_heap_words`, ADR 0088), and the runtime addresses at most
/// `u32::MAX` heap words: a larger capacity would be clamped to that silently,
/// so it is refused here instead. The default, without `max_heap_words`, is
/// the runtime's own (four mebiwords, 32 MiB).
pub const HEAP_CEILING_WORDS: u64 = u32::MAX as u64;

/// `app.toml` as written.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppFile {
    /// `module.function`; `<app>.handle` when absent.
    pub entry: Option<String>,
    /// The capabilities this app is granted, and nothing else.
    #[serde(default)]
    pub grant: Vec<String>,
    #[serde(default)]
    pub limits: LimitsFile,
    #[serde(default)]
    pub kv: KvFile,
    #[serde(default)]
    pub fetch: FetchFile,
    #[serde(default)]
    pub assets: AssetsFile,
    #[serde(default)]
    pub sql: SqlFile,
    #[serde(default)]
    pub rate: RateFile,
    #[serde(default)]
    pub defer: DeferFile,
    /// Named secrets the app may check a presented credential against
    /// (`auth.check`), or the host sends as a header (`[fetch.headers]`);
    /// never read.
    #[serde(default)]
    pub secrets: BTreeMap<String, SecretFile>,
    #[serde(default)]
    pub route: RouteFile,
    /// `[[triggers]]`: when the host runs the app itself
    /// ([`crate::triggers`]).
    #[serde(default)]
    pub triggers: Vec<TriggerFile>,
    /// Cloudflare Access, for `auth.identity` ([`crate::access`]).
    #[serde(default)]
    pub access: AccessFile,
}

/// `[access]` as written.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccessFile {
    /// The Access team domain, `<team>.cloudflareaccess.com` (or a URL).
    pub team: Option<SecretFile>,
    /// The Access application's AUD tag; several, separated by commas or
    /// spaces, for an app behind more than one Access application.
    pub aud: Option<SecretFile>,
    /// Only these emails, separated by commas or spaces; any Access let
    /// through when unset.
    pub emails: Option<SecretFile>,
    /// The `[secrets]` entry `auth.identity` accepts as a token.
    pub token: Option<String>,
    /// `"none"` (the default): with Access on, only an Access token gets in.
    /// `"token"`: the `token` secret as well.
    pub fallback: Option<String>,
}

/// `[route]` as written.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteFile {
    /// Hostnames that reach this app with the whole path. An app that has
    /// any is reached by them only: `/<name>/` on another hostname is not
    /// it.
    #[serde(default)]
    pub hosts: Vec<String>,
}

/// One `[[triggers]]` entry as written.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TriggerFile {
    /// A cron expression, or one of `@hourly`, `@daily`, `@weekly`,
    /// `@monthly`, `@yearly` ([`crate::cron`]).
    pub cron: String,
    /// The path the run is given; `/` by default.
    pub path: Option<String>,
    /// The method the run is given; `POST` by default.
    pub method: Option<String>,
}

/// How many schedules one app may have.
///
/// A tick asks every app's schedules whether they are due, so the cost of a
/// schedule is paid every minute whether it fires or not; an app that wants
/// more than this wants one schedule and a path that decides.
pub const MAX_TRIGGERS: usize = 16;

/// When the host runs an app itself, and as what.
///
/// The run is the app's own entry over a request the host builds, so an app
/// routes a trigger as it routes anything else; what it cannot do is
/// mistake one for a request from outside, because the host sets
/// `x-minicloud-trigger` on the one and strips it from the other
/// ([`crate::triggers`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Trigger {
    pub cron: crate::cron::Schedule,
    /// Where the run is sent, below the app's prefix: starts with `/`.
    pub path: String,
    /// The method the run is made with.
    pub method: String,
}

/// One `[secrets]` entry: where its value comes from.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretFile {
    /// An environment variable of the host process.
    pub env: Option<String>,
    /// A file, relative to the app's directory unless absolute; trailing
    /// whitespace is trimmed.
    pub file: Option<String>,
    /// The value itself, for tests and demos; prefer `env` or `file`.
    pub value: Option<String>,
    /// A name in the host's secret store ([`crate::secrets`]), set by the
    /// admin at run time. Secrets only: an `[access]` setting cannot come
    /// from the store.
    pub store: Option<String>,
}

/// Where `{ store = "…" }` secrets are looked up, and what a secret that
/// cannot be resolved becomes.
#[derive(Clone, Debug, Default)]
pub struct SecretSource {
    /// The host's store; without one, a `store` secret is unset.
    pub store: Option<Arc<SecretStore>>,
    /// `minicloud test`: a secret that cannot be resolved — its variable
    /// unset, its file missing, its store entry not set — is given
    /// [`placeholder`] as its value instead of refusing the app, and
    /// [`AppConfig::placeholders`] names it. Off everywhere a request is
    /// served.
    pub placeholders: bool,
}

impl SecretSource {
    /// Looking `store` secrets up in `store`.
    pub fn store(store: Arc<SecretStore>) -> SecretSource {
        SecretSource {
            store: Some(store),
            placeholders: false,
        }
    }
}

/// The value `minicloud test` gives a secret it cannot resolve.
pub fn placeholder(name: &str) -> String {
    format!("minicloud-test-placeholder-{name}")
}

/// An app's secrets, by name, resolved. `Debug` prints the names only.
#[derive(Clone, Default)]
pub struct Secrets(pub BTreeMap<String, String>);

impl std::fmt::Debug for Secrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_set().entries(self.0.keys()).finish()
    }
}

/// Resolves every secret, or says which could not be; with
/// [`SecretSource::placeholders`], also the names given a placeholder.
fn resolve_secrets(
    entries: BTreeMap<String, SecretFile>,
    dir: Option<&Path>,
    source: &SecretSource,
) -> Result<(Secrets, Vec<String>), String> {
    let mut secrets = BTreeMap::new();
    let mut placeholders = Vec::new();
    for (name, entry) in entries {
        let resolved = match (entry.env, entry.file, entry.value, entry.store) {
            (Some(var), None, None, None) => std::env::var(&var).map_err(|_| {
                format!("secret `{name}`: the environment variable `{var}` is not set")
            }),
            (None, Some(file), None, None) => {
                let path = match dir {
                    Some(dir) if !Path::new(&file).is_absolute() => dir.join(&file),
                    _ => std::path::PathBuf::from(&file),
                };
                std::fs::read_to_string(&path)
                    .map(|text| text.trim_end().to_string())
                    .map_err(|e| format!("secret `{name}`: cannot read `{}`: {e}", path.display()))
            }
            (None, None, Some(value), None) => Ok(value),
            (None, None, None, Some(stored)) => {
                crate::secrets::valid_name(&stored)
                    .map_err(|why| format!("secret `{name}`: `store`: {why}"))?;
                match &source.store {
                    Some(store) => store.get(&stored).ok_or_else(|| {
                        format!(
                            "secret `{name}`: the host's secret store has no `{stored}` (set it \
                             on the admin app's Secrets page, or with `minicloud secret set \
                             {stored}`)"
                        )
                    }),
                    None => Err(format!(
                        "secret `{name}`: `{{ store = \"{stored}\" }}` needs the host's secret \
                         store, and there is none here (pass `--data`)"
                    )),
                }
            }
            _ => {
                return Err(format!(
                    "secret `{name}` must have exactly one of `env`, `file`, `value` or `store`"
                ))
            }
        };
        let value = match resolved {
            Ok(value) if !value.is_empty() => value,
            Ok(_) if !source.placeholders => return Err(format!("secret `{name}` is empty")),
            Err(why) if !source.placeholders => return Err(why),
            _ => {
                placeholders.push(name.clone());
                placeholder(&name)
            }
        };
        secrets.insert(name, value);
    }
    Ok((Secrets(secrets), placeholders))
}

/// A non-secret setting from the same three places as a secret: `None` when
/// its environment variable is unset or empty, or it is empty.
fn resolve_setting(
    key: &str,
    entry: Option<SecretFile>,
    dir: Option<&Path>,
) -> Result<Option<String>, String> {
    let Some(entry) = entry else {
        return Ok(None);
    };
    if entry.store.is_some() {
        return Err(format!(
            "`{key}` is a setting, not a secret: it cannot come from the secret store"
        ));
    }
    let value = match (entry.env, entry.file, entry.value) {
        (Some(var), None, None) => std::env::var(&var).unwrap_or_default(),
        (None, Some(file), None) => {
            let path = match dir {
                Some(dir) if !Path::new(&file).is_absolute() => dir.join(&file),
                _ => std::path::PathBuf::from(&file),
            };
            std::fs::read_to_string(&path)
                .map_err(|e| format!("`{key}`: cannot read `{}`: {e}", path.display()))?
        }
        (None, None, Some(value)) => value,
        _ => {
            return Err(format!(
                "`{key}` must have exactly one of `env`, `file` or `value`"
            ))
        }
    };
    let value = value.trim();
    Ok((!value.is_empty()).then(|| value.to_string()))
}

/// A list setting: split on commas and whitespace.
fn words(text: Option<String>) -> Vec<String> {
    text.unwrap_or_default()
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|word| !word.is_empty())
        .map(str::to_string)
        .collect()
}

/// Resolves `[access]` against the app's secrets.
fn resolve_access(
    file: AccessFile,
    secrets: &Secrets,
    dir: Option<&Path>,
) -> Result<crate::access::Access, String> {
    use crate::access::{Access, AccessPolicy};
    if let Some(token) = &file.token {
        if !secrets.0.contains_key(token) {
            return Err(format!(
                "`access.token = \"{token}\"` names no `[secrets]` entry"
            ));
        }
    }
    let fallback = match file.fallback.as_deref() {
        None | Some("none") => false,
        Some("token") => true,
        Some(other) => {
            return Err(format!(
                "`access.fallback = \"{other}\"` is not \"none\" or \"token\""
            ))
        }
    };
    if fallback && file.token.is_none() {
        return Err("`access.fallback = \"token\"` needs `access.token`".to_string());
    }
    let declared = file.team.is_some() || file.aud.is_some();
    let team = resolve_setting("access.team", file.team, dir)?;
    let audiences = words(resolve_setting("access.aud", file.aud, dir)?);
    let emails = words(resolve_setting("access.emails", file.emails, dir)?)
        .into_iter()
        .map(|email| email.to_ascii_lowercase())
        .collect();
    let (jwt, off) = match (team, audiences.is_empty()) {
        (Some(team), false) => (Some(AccessPolicy::new(&team, audiences, emails)?), None),
        _ if declared => (
            None,
            Some("`[access]` has no team or no aud here (unset in the environment?)".to_string()),
        ),
        _ => (None, None),
    };
    Ok(Access {
        jwt,
        token: file.token,
        fallback,
        off,
    })
}

/// `[kv]` as written.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KvFile {
    pub max_key_bytes: Option<usize>,
    pub max_value_bytes: Option<usize>,
    pub max_keys: Option<u64>,
    pub max_bytes: Option<u64>,
}

/// `[defer]` as written.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeferFile {
    pub max_in_flight: Option<u64>,
}

/// `[rate]` as written.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateFile {
    pub requests: Option<u32>,
    pub per: Option<String>,
    pub burst: Option<u32>,
    pub key: Option<String>,
}

/// `[sql]` as written.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SqlFile {
    pub max_bytes: Option<u64>,
    pub max_rows: Option<u64>,
    pub max_result_bytes: Option<u64>,
    pub max_statement_ms: Option<u64>,
    pub max_statement_bytes: Option<u64>,
}

/// `[assets]` as written.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssetsFile {
    pub max_bytes: Option<u64>,
    pub cache_control: Option<String>,
}

/// `[fetch]` as written.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FetchFile {
    #[serde(default)]
    pub allow: Vec<String>,
    pub timeout: Option<String>,
    pub max_request_bytes: Option<usize>,
    pub max_response_bytes: Option<usize>,
    /// `[fetch.headers."<origin>"]`: per origin, headers whose value is a
    /// secret, added by the host when a request to that origin is sent.
    #[serde(default)]
    pub headers: BTreeMap<String, BTreeMap<String, HeaderFile>>,
}

/// One `[fetch.headers."<origin>"]` entry: `name = { secret = "…" }`, with
/// an optional literal `prefix` (`"Bearer "`) put before the secret.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeaderFile {
    /// A `[secrets]` name.
    pub secret: String,
    /// Sent before the secret, as written; empty by default.
    #[serde(default)]
    pub prefix: String,
}

/// A header the host adds to every request sent to one origin, its value
/// built from a secret. The app never sees the value: `Debug` prints the
/// origin, the header's name and the secret's name only.
#[derive(Clone)]
pub struct InjectedHeader {
    /// The origin, as `[fetch] allow` parses it; its port is never `*`.
    pub origin: AllowRule,
    /// Lowercase.
    pub name: reqwest::header::HeaderName,
    /// The `[secrets]` name the value comes from.
    pub secret: String,
    /// The prefix and the secret, marked sensitive.
    pub value: reqwest::header::HeaderValue,
}

impl std::fmt::Debug for InjectedHeader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} {}: <secret `{}`>",
            self.origin, self.name, self.secret
        )
    }
}

impl InjectedHeader {
    /// Whether a request to `scheme://host:port` gets this header: the
    /// origin exactly — scheme, host and effective port.
    pub fn applies_to(&self, scheme: &str, host: &str, port: u16) -> bool {
        self.origin.admits(scheme, host, port)
    }
}

/// Resolves `[fetch.headers]` against the allowlist as `app.toml` writes it
/// and against the app's secrets. Nothing said here quotes a secret's value.
fn resolve_injected_headers(
    headers: BTreeMap<String, BTreeMap<String, HeaderFile>>,
    written_allow: &[String],
    secrets: &Secrets,
) -> Result<Vec<InjectedHeader>, String> {
    let allowed: Vec<AllowRule> = written_allow
        .iter()
        .filter_map(|entry| AllowRule::parse(entry).ok())
        .collect();
    let mut injected = Vec::new();
    for (origin_text, entries) in headers {
        let at = format!("`fetch.headers.\"{origin_text}\"`");
        let origin = AllowRule::parse(&origin_text)
            .ok()
            .filter(|rule| rule.port.is_some())
            .ok_or_else(|| {
                format!(
                    "{at}: `{origin_text}` is not an origin like \"https://api.openai.com\" \
                     (a scheme, a host and an optional port; no path, no `:*`)"
                )
            })?;
        if !allowed.contains(&origin) {
            return Err(format!(
                "{at}: `{origin_text}` is not an entry of `[fetch] allow`; a secret header goes \
                 only to an origin the allowlist names exactly"
            ));
        }
        for (name, entry) in entries {
            let header = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| format!("{at}: `{name}` is not a header name"))?;
            let Some(secret) = secrets.0.get(&entry.secret) else {
                return Err(format!(
                    "{at}: `{name} = {{ secret = \"{}\" }}` names no `[secrets]` entry",
                    entry.secret
                ));
            };
            if reqwest::header::HeaderValue::from_str(&entry.prefix).is_err() {
                return Err(format!(
                    "{at}: the `prefix` of `{name}` cannot be sent in a header value"
                ));
            }
            let mut value =
                reqwest::header::HeaderValue::from_str(&format!("{}{secret}", entry.prefix))
                    .map_err(|_| {
                        format!(
                            "{at}: secret `{}` cannot be sent as the value of `{name}` \
                             (a control character or a line break?)",
                            entry.secret
                        )
                    })?;
            value.set_sensitive(true);
            injected.push(InjectedHeader {
                origin: origin.clone(),
                name: header,
                secret: entry.secret,
                value,
            });
        }
    }
    Ok(injected)
}

/// An app's key-value quotas.
#[derive(Clone, Debug)]
pub struct KvLimits {
    /// The longest key, in bytes.
    pub max_key_bytes: usize,
    /// The largest value, in bytes.
    pub max_value_bytes: usize,
    /// How many keys the app may hold.
    pub max_keys: u64,
    /// The keys' and values' bytes, summed, the app may hold.
    pub max_bytes: u64,
}

impl Default for KvLimits {
    fn default() -> KvLimits {
        KvLimits {
            max_key_bytes: 1024,
            max_value_bytes: 1 << 20,
            max_keys: 100_000,
            max_bytes: 64 << 20,
        }
    }
}

/// One `[fetch] allow` entry: `scheme://host`, `scheme://host:port` or
/// `scheme://host:*`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AllowRule {
    pub scheme: String,
    pub host: String,
    /// `None` is any port.
    pub port: Option<u16>,
}

impl AllowRule {
    /// Parses one entry. The port defaults to the scheme's (80 or 443).
    pub fn parse(text: &str) -> Result<AllowRule, String> {
        let bad = || {
            format!(
                "`{text}` is not an allowlist entry like \"https://api.example.com\", \
                 \"http://127.0.0.1:8080\" or \"http://localhost:*\""
            )
        };
        let (scheme, rest) = text.split_once("://").ok_or_else(bad)?;
        let scheme = scheme.to_ascii_lowercase();
        let default_port = match scheme.as_str() {
            "http" => 80,
            "https" => 443,
            _ => return Err(bad()),
        };
        if rest.is_empty() || rest.contains('/') || rest.contains('@') {
            return Err(bad());
        }
        let (host, port) = match rest.rsplit_once(':') {
            Some((host, "*")) => (host, None),
            Some((host, port)) => (host, Some(port.parse().map_err(|_| bad())?)),
            None => (rest, Some(default_port)),
        };
        if host.is_empty() {
            return Err(bad());
        }
        Ok(AllowRule {
            scheme,
            host: host.to_ascii_lowercase(),
            port,
        })
    }

    /// Whether a request to `scheme://host:port` is allowed by this rule.
    pub fn admits(&self, scheme: &str, host: &str, port: u16) -> bool {
        self.scheme == scheme
            && self.host.eq_ignore_ascii_case(host)
            && self.port.is_none_or(|allowed| allowed == port)
    }
}

impl std::fmt::Display for AllowRule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.port {
            Some(port) => write!(f, "{}://{}:{port}", self.scheme, self.host),
            None => write!(f, "{}://{}:*", self.scheme, self.host),
        }
    }
}

/// Where an app's `fetch` may go, and how much it may move.
#[derive(Clone, Debug)]
pub struct FetchPolicy {
    /// Empty: nowhere.
    pub allow: Vec<AllowRule>,
    /// One fetch, connect to last byte. The run's deadline also bounds it.
    pub timeout: Duration,
    pub max_request_bytes: usize,
    pub max_response_bytes: usize,
    /// `[fetch.headers]`, resolved: what the host adds to a request to each
    /// origin. Not an override's to change.
    pub headers: Vec<InjectedHeader>,
}

impl Default for FetchPolicy {
    fn default() -> FetchPolicy {
        FetchPolicy {
            allow: Vec::new(),
            timeout: Duration::from_secs(10),
            max_request_bytes: 1 << 20,
            max_response_bytes: 4 << 20,
            headers: Vec::new(),
        }
    }
}

impl FetchPolicy {
    /// Whether `scheme://host:port` is on the allowlist.
    pub fn admits(&self, scheme: &str, host: &str, port: u16) -> bool {
        self.allow
            .iter()
            .any(|rule| rule.admits(scheme, host, port))
    }
}

/// `[limits]` as written; also what an override sets, key by key.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LimitsFile {
    /// `fuel`, which Cove's ADR 0091 removed: read only so that it can be
    /// refused or ignored by name ([`RemovedKeys`]) rather than as an unknown
    /// key, and never written.
    #[serde(default, skip_serializing)]
    pub fuel: Option<Removed>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_host_calls: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_call_depth: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_heap_words: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_in_flight: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_queued: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_request_bytes: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_response_bytes: Option<usize>,
}

impl LimitsFile {
    /// Whether no key is set.
    pub fn is_empty(&self) -> bool {
        *self == LimitsFile::default()
    }

    /// `self`, with every key `over` sets replaced.
    fn overlaid(self, over: &LimitsFile) -> LimitsFile {
        LimitsFile {
            max_host_calls: over.max_host_calls.or(self.max_host_calls),
            deadline: over.deadline.clone().or(self.deadline),
            max_call_depth: over.max_call_depth.or(self.max_call_depth),
            max_heap_words: over.max_heap_words.or(self.max_heap_words),
            max_in_flight: over.max_in_flight.or(self.max_in_flight),
            max_queued: over.max_queued.or(self.max_queued),
            max_request_bytes: over.max_request_bytes.or(self.max_request_bytes),
            max_response_bytes: over.max_response_bytes.or(self.max_response_bytes),
            ..LimitsFile::default()
        }
    }
}

/// What the admin app changed of an app's `app.toml`, kept in the data
/// directory rather than in the file: a deploy replaces the app's directory
/// with what was sent, and the change has to outlive it.
///
/// Sets are kept as what was added and removed relative to the file, not as
/// the whole set, so that a release whose `app.toml` grants something new
/// still grants it — unless that very capability was removed here.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AppOverride {
    /// `false`: the app is loaded but not routed to.
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub grant_add: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub grant_remove: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fetch_allow_add: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fetch_allow_remove: Vec<String>,
    #[serde(default, skip_serializing_if = "LimitsFile::is_empty")]
    pub limits: LimitsFile,
}

fn yes() -> bool {
    true
}

fn is_true(value: &bool) -> bool {
    *value
}

impl AppOverride {
    /// The default: enabled, nothing changed.
    pub fn none() -> AppOverride {
        AppOverride {
            enabled: true,
            ..AppOverride::default()
        }
    }

    /// Whether this changes nothing.
    pub fn is_empty(&self) -> bool {
        *self == AppOverride::none()
    }

    /// Whether it changes the configuration (anything but `enabled`).
    pub fn changes_config(&self) -> bool {
        AppOverride {
            enabled: true,
            ..self.clone()
        } != AppOverride::none()
    }

    /// The override that turns the app's own configuration — `file`, as
    /// its `app.toml` reads with no override — into `grant`, `allow` and
    /// `limits`; `enabled` as given. A limit equal to what the file already
    /// comes to is no change and is not kept, so that a later release's
    /// `app.toml` is not masked by a value nobody changed.
    pub fn between(
        file: &AppConfig,
        grant: &BTreeSet<String>,
        allow: &[String],
        limits: &LimitsFile,
        enabled: bool,
    ) -> AppOverride {
        let file_allow: BTreeSet<String> = file.file_allow.iter().cloned().collect();
        let allow: BTreeSet<String> = allow.iter().cloned().collect();
        let theirs = file.limits.as_file();
        let differs = |mine: &Option<u64>, theirs: &Option<u64>| mine.filter(|_| mine != theirs);
        let differs_usize =
            |mine: &Option<usize>, theirs: &Option<usize>| mine.filter(|_| mine != theirs);
        let limits = LimitsFile {
            max_host_calls: differs(&limits.max_host_calls, &theirs.max_host_calls),
            deadline: limits.deadline.clone().filter(|mine| {
                let parsed = |text: &Option<String>| {
                    text.as_deref().and_then(|text| parse_duration(text).ok())
                };
                parsed(&Some(mine.clone())) != parsed(&theirs.deadline)
            }),
            max_call_depth: differs_usize(&limits.max_call_depth, &theirs.max_call_depth),
            max_heap_words: differs(&limits.max_heap_words, &theirs.max_heap_words),
            max_in_flight: differs_usize(&limits.max_in_flight, &theirs.max_in_flight),
            max_queued: differs_usize(&limits.max_queued, &theirs.max_queued),
            max_request_bytes: differs_usize(&limits.max_request_bytes, &theirs.max_request_bytes),
            max_response_bytes: differs_usize(
                &limits.max_response_bytes,
                &theirs.max_response_bytes,
            ),
            ..LimitsFile::default()
        };
        AppOverride {
            enabled,
            grant_add: grant.difference(&file.granted).cloned().collect(),
            grant_remove: file.granted.difference(grant).cloned().collect(),
            fetch_allow_add: allow.difference(&file_allow).cloned().collect(),
            fetch_allow_remove: file_allow.difference(&allow).cloned().collect(),
            limits,
        }
    }

    /// `file` with this override applied.
    fn apply(&self, mut file: AppFile) -> AppFile {
        let mut grant: BTreeSet<String> = file.grant.into_iter().collect();
        grant.extend(self.grant_add.iter().cloned());
        for gone in &self.grant_remove {
            grant.remove(gone);
        }
        file.grant = grant.into_iter().collect();
        let mut allow = file.fetch.allow;
        for added in &self.fetch_allow_add {
            if !allow.contains(added) {
                allow.push(added.clone());
            }
        }
        allow.retain(|entry| !self.fetch_allow_remove.contains(entry));
        file.fetch.allow = allow;
        file.limits = std::mem::take(&mut file.limits).overlaid(&self.limits);
        file
    }
}

/// What bounds one app, with the defaults filled in.
#[derive(Clone, Debug)]
pub struct AppLimits {
    /// Each request's run: deadline, host calls, call depth, tasks.
    pub run: Limits,
    /// The capacity of each run's heap, in words: a run that needs more fails
    /// the allocation ("this run has no memory left", 500, `heap` in
    /// `/_host/stats`) while it runs, not after (ADR 0088). `None` is the
    /// runtime's default; the hard ceiling is [`HEAP_CEILING_WORDS`].
    pub max_heap_words: Option<u64>,
    /// Runs of this app started and not yet answered — running, parked or
    /// waiting to continue. A request beyond it waits in the queue.
    pub max_in_flight: usize,
    /// Requests of this app waiting to start. A request beyond it is
    /// rejected with 429.
    pub max_queued: usize,
    /// The largest request body accepted; larger is 413.
    pub max_request_bytes: usize,
    /// The largest response body an app may answer; larger is 500.
    pub max_response_bytes: usize,
}

impl Default for AppLimits {
    fn default() -> AppLimits {
        AppLimits {
            run: Limits {
                deadline: Some(Duration::from_secs(10)),
                max_host_calls: Some(1_000),
                max_call_depth: None,
                // `spawn` is refused when the app is loaded (see
                // `apps::refuse_spawn`); this is the runtime's own backstop
                // for it, stopping a run whose first `spawn` got past that.
                max_tasks: Some(0),
            },
            max_heap_words: None,
            max_in_flight: 64,
            max_queued: 256,
            max_request_bytes: 1 << 20,
            max_response_bytes: 4 << 20,
        }
    }
}

impl AppLimits {
    /// These limits as `[limits]` would write them, every key set but a
    /// heap limit that is not.
    pub fn as_file(&self) -> LimitsFile {
        LimitsFile {
            max_host_calls: self.run.max_host_calls,
            deadline: self
                .run
                .deadline
                .map(|deadline| format!("{}ms", deadline.as_millis())),
            max_call_depth: self.run.max_call_depth,
            max_heap_words: self.max_heap_words,
            max_in_flight: Some(self.max_in_flight),
            max_queued: Some(self.max_queued),
            max_request_bytes: Some(self.max_request_bytes),
            max_response_bytes: Some(self.max_response_bytes),
            ..LimitsFile::default()
        }
    }
}

/// Whether `name` is a header name a proxy could set.
fn is_header_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// A limit that is a count of something, which 0 never is: a limit of 0
/// would mean an app that may do nothing at all, which is what not granting
/// the capability says, and says better.
fn positive(section: &str, key: &str, written: Option<u64>, fallback: u64) -> Result<u64, String> {
    match written {
        Some(0) => Err(format!(
            "`[{section}] {key}` is 0; leave the capability out of `grant` to take it away"
        )),
        Some(value) => Ok(value),
        None => Ok(fallback),
    }
}

/// `[assets]`: how the host answers the files in the app's `assets/`
/// directory.
///
/// An app can only return UTF-8 strings, so an image or a stylesheet cannot
/// come out of its handler; the host serves the files of `assets/` itself,
/// and this is how.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssetPolicy {
    /// The files together; an app whose `assets/` is larger is refused at
    /// load. 8 MiB by default: the files are all held in memory once loaded,
    /// and while an app is being updated the old version and the new one
    /// are alive at the same time, so the default is kept modest. The
    /// archive limits of a deploy (16 MiB, 4096 files) apply as well, and
    /// are also the largest value this accepts.
    pub max_bytes: u64,
    /// The `cache-control` every file is answered with. `no-cache` by
    /// default: the files change together with the app's version, and a
    /// stale stylesheet left behind after a deploy does more harm than a
    /// request to revalidate costs, which with an etag is cheap (a 304, no
    /// body). It is the key to turn when the file names carry a hash, to
    /// something like `public, max-age=31536000, immutable`.
    pub cache_control: String,
}

impl Default for AssetPolicy {
    fn default() -> AssetPolicy {
        AssetPolicy {
            max_bytes: 8 << 20,
            cache_control: "no-cache".to_string(),
        }
    }
}

/// `[sql]`: what the app's SQL store may hold and what one statement may
/// cost ([`crate::sql`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SqlLimits {
    /// The database, in bytes. 64 MiB by default, as `[kv] max_bytes` is:
    /// it is the same disk and the same reason, that one app's store should
    /// not be able to fill it.
    pub max_bytes: u64,
    /// Rows one query may answer. A query past it is the app's `Err` rather
    /// than a truncated answer, because a page that silently showed the
    /// first thousand of something would be wrong rather than short.
    pub max_rows: u64,
    /// The values of one answer, summed. The run's `max_response_bytes`
    /// bounds what it may then send; this bounds what it may hold.
    pub max_result_bytes: u64,
    /// How long one statement may run.
    ///
    /// SQLite answers on the worker, as `kv` does, so this is a worker held
    /// and not yielded: well above the 2 ms slice, and deliberately small
    /// for that reason. A statement that runs past it is stopped where it
    /// is (SQLite's progress handler) and is the app's `Err`; an app whose
    /// query needs longer wants an index, not a larger number here.
    pub max_statement_ms: u64,
    /// How long a statement's text may be.
    pub max_statement_bytes: u64,
}

impl Default for SqlLimits {
    fn default() -> SqlLimits {
        SqlLimits {
            max_bytes: 64 << 20,
            max_rows: 1000,
            max_result_bytes: 4 << 20,
            max_statement_ms: 20,
            max_statement_bytes: 64 << 10,
        }
    }
}

/// `[defer]`: how much work an app may have outstanding
/// ([`crate::defer`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeferLimits {
    /// Deferred runs of this app that have not finished. A call past it is
    /// the app's `Err`: a queue of work that grows without end is a leak
    /// with a slower name. Eight by default, which is more than a request
    /// that defers one thing ever needs and few enough to notice.
    pub max_in_flight: u64,
}

impl Default for DeferLimits {
    fn default() -> DeferLimits {
        DeferLimits { max_in_flight: 8 }
    }
}

/// What a `[rate]` counts: who the tokens are spent by.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RateKey {
    /// The address the connection came from. Behind a proxy that is the
    /// proxy, which is why there is a header to name instead.
    Address,
    /// A header the proxy in front sets, such as `cf-connecting-ip`. The
    /// host does not verify it — a header is only as good as whoever is
    /// allowed to set it — so this is for a host that is behind something
    /// that overwrites it.
    Header(String),
    /// The app as a whole: one bucket for everyone.
    App,
}

/// `[rate]`: how often one caller may reach an app ([`crate::rate`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RatePolicy {
    pub requests: u32,
    pub per: Duration,
    pub burst: u32,
    pub key: RateKey,
}

/// An app's configuration, read and validated.
#[derive(Clone, Debug)]
pub struct AppConfig {
    pub entry: String,
    pub granted: BTreeSet<String>,
    pub limits: AppLimits,
    pub kv: KvLimits,
    pub assets: AssetPolicy,
    pub sql: SqlLimits,
    pub defer: DeferLimits,
    /// `[rate]`: how often one caller may reach this app; `None` for no
    /// limit of its own, which is the default.
    pub rate: Option<RatePolicy>,
    /// `[[triggers]]`: when the host runs this app itself.
    pub triggers: Vec<Trigger>,
    pub fetch: FetchPolicy,
    pub secrets: Secrets,
    /// The secrets given a placeholder value because they could not be
    /// resolved ([`SecretSource::placeholders`]; `minicloud test` only).
    pub placeholders: Vec<String>,
    /// `[route] hosts`, lower-cased.
    pub hosts: Vec<String>,
    /// `[fetch] allow` as written, after any override: the entries the
    /// admin app edits.
    pub file_allow: Vec<String>,
    /// `[access]`, resolved.
    pub access: crate::access::Access,
    /// What was read and ignored, one line each, for the host to log: a
    /// removed key in a file loaded under [`RemovedKeys::Ignore`].
    pub warnings: Vec<String>,
}

/// A key `app.toml` may no longer say, present: read as anything and kept as
/// nothing but the fact that it was there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Removed;

impl<'de> Deserialize<'de> for Removed {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Removed, D::Error> {
        serde::de::IgnoredAny::deserialize(deserializer).map(|_| Removed)
    }
}

/// What reading an `app.toml` does with a key that was removed —
/// `limits.fuel`, since Cove's ADR 0091.
///
/// A file being put forward (`minicloud check` and `test`, a deploy, an
/// update) is refused, naming the key and what replaces it: the person who
/// wrote it is there to fix it. A file that is already deployed and is being
/// loaded again (the host's start, a rollback, an admin change, a secret
/// reload, `minicloud check --deployed` as `install.sh` runs it before an
/// upgrade) has the key ignored, with a warning: it was valid when it was
/// deployed, and refusing it would take a working app down on the host's
/// upgrade.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemovedKeys {
    Refuse,
    Ignore,
}

/// Why `limits.fuel` is refused.
pub const FUEL_REMOVED: &str = "`limits.fuel` was removed (Cove ADR 0091 took the fuel allowance \
     out of the runtime); bound a request with `limits.deadline` instead";

/// Reads `dir/app.toml` for the app `name`.
pub fn read_app(dir: &Path, name: &str) -> Result<AppConfig, String> {
    read_app_with(
        dir,
        name,
        None,
        &SecretSource::default(),
        RemovedKeys::Refuse,
    )
}

/// [`read_app`], with `over` applied to the file before it is validated,
/// `store` secrets looked up in `source`, and a removed key treated as
/// `removed` says.
pub fn read_app_with(
    dir: &Path,
    name: &str,
    over: Option<&AppOverride>,
    source: &SecretSource,
    removed: RemovedKeys,
) -> Result<AppConfig, String> {
    let path = dir.join("app.toml");
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("cannot read `{}`: {e}", path.display()))?;
    parse_app_with(&text, name, Some(dir), over, source, removed).map_err(|e| match over {
        Some(over) if over.changes_config() => {
            format!("`{}` with the admin's changes: {e}", path.display())
        }
        _ => format!("`{}`: {e}", path.display()),
    })
}

/// Parses an `app.toml` for the app `name`.
pub fn parse_app(text: &str, name: &str) -> Result<AppConfig, String> {
    parse_app_in(text, name, None)
}

/// [`parse_app`], with `file` secrets read relative to `dir`.
pub fn parse_app_in(text: &str, name: &str, dir: Option<&Path>) -> Result<AppConfig, String> {
    parse_app_with(
        text,
        name,
        dir,
        None,
        &SecretSource::default(),
        RemovedKeys::Refuse,
    )
}

/// [`parse_app_in`], with `over` applied to the file before anything is
/// validated, `store` secrets looked up in `source`, and a removed key
/// treated as `removed` says.
pub fn parse_app_with(
    text: &str,
    name: &str,
    dir: Option<&Path>,
    over: Option<&AppOverride>,
    source: &SecretSource,
    removed: RemovedKeys,
) -> Result<AppConfig, String> {
    let mut file: AppFile = toml::from_str(text).map_err(|e| e.message().to_string())?;
    let mut warnings = Vec::new();
    if file.limits.fuel.take().is_some() {
        match removed {
            RemovedKeys::Refuse => return Err(FUEL_REMOVED.to_string()),
            RemovedKeys::Ignore => warnings.push(format!(
                "`limits.fuel` in app.toml is ignored: Cove ADR 0091 removed the fuel \
                 allowance, and `{name}`'s requests are bounded by `limits.deadline`; drop the \
                 key when the app is next deployed, which will refuse it"
            )),
        }
    }
    // `[fetch.headers]` is held to the allowlist as the file writes it, not
    // as an override leaves it: an override can take an origin off the list
    // (its requests are then refused, header and all) but cannot give a
    // secret header an origin `app.toml` did not.
    let written_allow = file.fetch.allow.clone();
    let file = match over {
        Some(over) => over.apply(file),
        None => file,
    };
    let defaults = AppLimits::default();
    let l = file.limits;
    let deadline = match l.deadline {
        Some(text) => Some(parse_duration(&text)?),
        None => defaults.run.deadline,
    };
    if let Some(words) = l.max_heap_words {
        if words > HEAP_CEILING_WORDS {
            return Err(format!(
                "`limits.max_heap_words = {words}` is above the largest heap a run can have, \
                 {HEAP_CEILING_WORDS} words"
            ));
        }
    }
    let at_least_one = |key: &str, value: Option<usize>, default: usize| match value {
        Some(0) => Err(format!("`limits.{key}` must be at least 1")),
        Some(n) => Ok(n),
        None => Ok(default),
    };
    let limits = AppLimits {
        run: Limits {
            deadline,
            max_host_calls: l.max_host_calls.or(defaults.run.max_host_calls),
            max_call_depth: l.max_call_depth.or(defaults.run.max_call_depth),
            max_tasks: defaults.run.max_tasks,
        },
        max_heap_words: l.max_heap_words,
        max_in_flight: at_least_one("max_in_flight", l.max_in_flight, defaults.max_in_flight)?,
        max_queued: l.max_queued.unwrap_or(defaults.max_queued),
        max_request_bytes: l.max_request_bytes.unwrap_or(defaults.max_request_bytes),
        max_response_bytes: l.max_response_bytes.unwrap_or(defaults.max_response_bytes),
    };
    if limits
        .run
        .deadline
        .is_some_and(|deadline| deadline.is_zero())
    {
        return Err("`limits.deadline` must be longer than 0 ms".to_string());
    }
    for capability in &file.grant {
        if capability.is_empty()
            || !capability
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return Err(format!("`grant`: `{capability}` is not a capability name"));
        }
    }
    if file.grant.iter().any(|c| c == ADMIN_CAPABILITY) && name != ADMIN_APP {
        return Err(format!(
            "`grant` names `{ADMIN_CAPABILITY}`, which only the app `{ADMIN_APP}` may be \
             granted: it can change every app's configuration"
        ));
    }
    let mut hosts = Vec::new();
    for host in &file.route.hosts {
        let host = host.to_ascii_lowercase();
        let valid = !host.is_empty()
            && host.len() <= 253
            && host.split('.').all(|label| {
                !label.is_empty() && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            });
        if !valid {
            return Err(format!(
                "`route.hosts`: `{host}` is not a hostname (no scheme, port or path)"
            ));
        }
        if !hosts.contains(&host) {
            hosts.push(host);
        }
    }
    if file.triggers.len() > MAX_TRIGGERS {
        return Err(format!(
            "`[[triggers]]`: {} schedules, which is more than {MAX_TRIGGERS}",
            file.triggers.len()
        ));
    }
    let mut triggers = Vec::new();
    for trigger in file.triggers {
        let cron = crate::cron::Schedule::parse(&trigger.cron)
            .map_err(|why| format!("`[[triggers]] cron`: {why}"))?;
        let path = trigger.path.unwrap_or_else(|| "/".to_string());
        // The path is a path the app would see, so it is held to what one
        // looks like: a query is the schedule's to put in the path it asks
        // for, not a field of its own.
        let usable = path.starts_with('/')
            && path.len() <= 1024
            && path
                .chars()
                .all(|c| c.is_ascii_graphic() && c != '?' && c != '#');
        if !usable {
            return Err(format!(
                "`[[triggers]] path = \"{path}\"` is not a path: it starts with `/` and holds \
                 no space, `?` or `#`"
            ));
        }
        let method = trigger.method.unwrap_or_else(|| "POST".to_string());
        if method.is_empty() || !method.bytes().all(|b| b.is_ascii_uppercase()) {
            return Err(format!(
                "`[[triggers]] method = \"{method}\"` is not a method: upper-case letters, \
                 such as `POST` or `GET`"
            ));
        }
        triggers.push(Trigger { cron, path, method });
    }
    let entry = file.entry.unwrap_or_else(|| format!("{name}.handle"));
    if entry.split_once('.').is_none() {
        return Err(format!("`entry = \"{entry}\"` is not `module.function`"));
    }
    let kv_defaults = KvLimits::default();
    let kv = KvLimits {
        max_key_bytes: file.kv.max_key_bytes.unwrap_or(kv_defaults.max_key_bytes),
        max_value_bytes: file
            .kv
            .max_value_bytes
            .unwrap_or(kv_defaults.max_value_bytes),
        max_keys: file.kv.max_keys.unwrap_or(kv_defaults.max_keys),
        max_bytes: file.kv.max_bytes.unwrap_or(kv_defaults.max_bytes),
    };
    // `[rate]` is off unless `requests` says how many: a window and a burst
    // with no count is not half a limit, it is a mistake.
    let rate = match (
        file.rate.requests,
        &file.rate.per,
        file.rate.burst,
        &file.rate.key,
    ) {
        (None, None, None, None) => None,
        (None, ..) => {
            return Err(
                "`[rate]` needs `requests`: how many a caller may make over `per`".to_string(),
            )
        }
        (Some(0), ..) => {
            return Err(
                "`[rate] requests` is 0; disable the app instead of refusing every caller"
                    .to_string(),
            )
        }
        (Some(requests), per, burst, key) => {
            let per = match per {
                Some(text) => {
                    let per = parse_duration(text).map_err(|why| format!("`[rate] per`: {why}"))?;
                    if per.is_zero() {
                        return Err("`[rate] per` is 0; a window has a length".to_string());
                    }
                    per
                }
                None => Duration::from_secs(60),
            };
            let key = match key.as_deref() {
                None | Some("address") => RateKey::Address,
                Some("app") => RateKey::App,
                Some(named) => match named.strip_prefix("header:") {
                    Some(header) if is_header_name(header) => {
                        RateKey::Header(header.to_ascii_lowercase())
                    }
                    _ => {
                        return Err(format!(
                            "`[rate] key = \"{named}\"` is not a key: `address`, `app`, or \
                             `header:<name>`"
                        ))
                    }
                },
            };
            Some(RatePolicy {
                requests,
                per,
                burst: burst.unwrap_or(requests).max(1),
                key,
            })
        }
    };
    let defer = DeferLimits {
        max_in_flight: positive(
            "defer",
            "max_in_flight",
            file.defer.max_in_flight,
            DeferLimits::default().max_in_flight,
        )?,
    };
    let sql_defaults = SqlLimits::default();
    let sql = SqlLimits {
        max_bytes: positive(
            "sql",
            "max_bytes",
            file.sql.max_bytes,
            sql_defaults.max_bytes,
        )?,
        max_rows: positive("sql", "max_rows", file.sql.max_rows, sql_defaults.max_rows)?,
        max_result_bytes: positive(
            "sql",
            "max_result_bytes",
            file.sql.max_result_bytes,
            sql_defaults.max_result_bytes,
        )?,
        max_statement_ms: positive(
            "sql",
            "max_statement_ms",
            file.sql.max_statement_ms,
            sql_defaults.max_statement_ms,
        )?,
        max_statement_bytes: positive(
            "sql",
            "max_statement_bytes",
            file.sql.max_statement_bytes,
            sql_defaults.max_statement_bytes,
        )?,
    };
    let asset_defaults = AssetPolicy::default();
    // No more than a deploy's archive can carry: a larger allowance could
    // never be used.
    let asset_ceiling = crate::deploy::Limits::standard().max_bytes;
    let assets = AssetPolicy {
        max_bytes: match file.assets.max_bytes {
            Some(0) => {
                return Err(
                    "`[assets] max_bytes` is 0; an app with no assets leaves `assets/` out"
                        .to_string(),
                )
            }
            Some(bytes) if bytes > asset_ceiling => {
                return Err(format!(
                    "`[assets] max_bytes = {bytes}` is above {asset_ceiling}, the most a \
                     deployed app can be, so the allowance could never be used"
                ))
            }
            Some(bytes) => bytes,
            None => asset_defaults.max_bytes,
        },
        cache_control: match file.assets.cache_control {
            Some(text) => {
                if text.trim().is_empty() {
                    return Err("`[assets] cache_control` is empty; leave it out for \
                         `no-cache`"
                        .to_string());
                }
                if !text.bytes().all(|b| (0x20..0x7f).contains(&b))
                    || reqwest::header::HeaderValue::from_str(&text).is_err()
                {
                    return Err(format!(
                        "`[assets] cache_control = {text:?}` cannot be sent as a header value; \
                         use printable ASCII only"
                    ));
                }
                text
            }
            None => asset_defaults.cache_control,
        },
    };
    let (secrets, placeholders) = resolve_secrets(file.secrets, dir, source)?;
    let fetch_defaults = FetchPolicy::default();
    let fetch = FetchPolicy {
        allow: file
            .fetch
            .allow
            .iter()
            .map(|entry| AllowRule::parse(entry).map_err(|why| format!("`fetch.allow`: {why}")))
            .collect::<Result<_, _>>()?,
        timeout: match file.fetch.timeout {
            Some(text) => parse_duration(&text)?,
            None => fetch_defaults.timeout,
        },
        max_request_bytes: file
            .fetch
            .max_request_bytes
            .unwrap_or(fetch_defaults.max_request_bytes),
        max_response_bytes: file
            .fetch
            .max_response_bytes
            .unwrap_or(fetch_defaults.max_response_bytes),
        headers: resolve_injected_headers(file.fetch.headers, &written_allow, &secrets)?,
    };
    let access = resolve_access(file.access, &secrets, dir)?;
    Ok(AppConfig {
        entry,
        granted: file.grant.into_iter().collect(),
        limits,
        kv,
        assets,
        sql,
        defer,
        rate,
        triggers,
        fetch,
        secrets,
        placeholders,
        hosts,
        file_allow: file.fetch.allow,
        access,
        warnings,
    })
}

/// The store names `dir/app.toml`'s `[secrets]` take a value from: whether
/// setting or deleting a stored secret concerns the app. Empty for a file
/// that does not read — an app refused for that is not using the store.
pub fn store_references(dir: &Path) -> BTreeSet<String> {
    std::fs::read_to_string(dir.join("app.toml"))
        .ok()
        .and_then(|text| toml::from_str::<AppFile>(&text).ok())
        .map(|file| {
            file.secrets
                .into_values()
                .filter_map(|entry| entry.store)
                .collect()
        })
        .unwrap_or_default()
}

/// `"300ms"`, `"5s"`, `"1m"` or `"2h"`.
///
/// Minutes and hours are here for `[rate] per`, where a window is
/// naturally one of them; a deadline may be written that way too, though
/// one that long is a strange deadline.
pub fn parse_duration(text: &str) -> Result<Duration, String> {
    const SHAPE: &str = "a duration like \"300ms\", \"5s\", \"1m\" or \"2h\"";
    let text = text.trim();
    // `ms` before `s`, or every `ms` would read as a number ending in `m`.
    let units = [("ms", 1u64), ("s", 1000), ("m", 60_000), ("h", 3_600_000)];
    let (number, unit) = units
        .iter()
        .find_map(|(suffix, unit)| text.strip_suffix(suffix).map(|number| (number, *unit)))
        .ok_or_else(|| format!("`{text}` is not {SHAPE}"))?;
    let n: u64 = number
        .trim()
        .parse()
        .map_err(|_| format!("`{text}` is not {SHAPE}"))?;
    Ok(Duration::from_millis(n * unit))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_file_is_every_default() {
        let config = parse_app("", "hello").unwrap();
        assert_eq!(config.entry, "hello.handle");
        assert!(config.granted.is_empty());
        assert_eq!(config.limits.run.deadline, Some(Duration::from_secs(10)));
        assert_eq!(config.limits.run.max_tasks, Some(0));
    }

    #[test]
    fn limits_are_read() {
        let config = parse_app(
            "grant = [\"log\"]\n[limits]\nmax_host_calls = 10\ndeadline = \"300ms\"\nmax_queued = 0\n",
            "a",
        )
        .unwrap();
        assert_eq!(config.limits.run.max_host_calls, Some(10));
        assert_eq!(config.limits.run.deadline, Some(Duration::from_millis(300)));
        assert_eq!(config.limits.max_queued, 0);
        assert!(config.granted.contains("log"));
    }

    #[test]
    fn an_unknown_key_is_refused() {
        let error = parse_app("[limits]\ndeadlin = \"5s\"\n", "a").unwrap_err();
        assert!(error.contains("deadlin"), "{error}");
    }

    #[test]
    fn a_file_put_forward_with_the_removed_fuel_limit_is_refused_by_name() {
        let error = parse_app("[limits]\nfuel = 2000000\n", "a").unwrap_err();
        assert_eq!(error, FUEL_REMOVED);
        assert!(error.contains("ADR 0091"), "{error}");
        assert!(error.contains("limits.deadline"), "{error}");
        // Not the generic refusal of a key nobody knows.
        assert!(!error.contains("unknown field"), "{error}");
    }

    #[test]
    fn a_deployed_file_with_the_removed_fuel_limit_loads_with_a_warning() {
        let config = parse_app_with(
            "[limits]\nfuel = 400000000\ndeadline = \"7s\"\n",
            "algo",
            None,
            None,
            &SecretSource::default(),
            RemovedKeys::Ignore,
        )
        .unwrap();
        assert_eq!(config.limits.run.deadline, Some(Duration::from_secs(7)));
        assert_eq!(config.warnings.len(), 1, "{:?}", config.warnings);
        let warning = &config.warnings[0];
        assert!(warning.contains("`limits.fuel`"), "{warning}");
        assert!(warning.contains("ADR 0091"), "{warning}");
        assert!(warning.contains("`algo`"), "{warning}");
        // Without it, nothing to say.
        let clean = parse_app_with(
            "[limits]\ndeadline = \"7s\"\n",
            "algo",
            None,
            None,
            &SecretSource::default(),
            RemovedKeys::Ignore,
        )
        .unwrap();
        assert!(clean.warnings.is_empty());
        // Whatever it said: an old file could only say a number, but the
        // key is gone either way.
        assert!(parse_app_with(
            "[limits]\nfuel = \"lots\"\n",
            "algo",
            None,
            None,
            &SecretSource::default(),
            RemovedKeys::Ignore,
        )
        .is_ok());
    }

    #[test]
    fn a_trigger_takes_its_path_and_method_or_their_defaults() {
        let config = parse_app("[[triggers]]\ncron = \"@daily\"\n", "a").unwrap();
        assert_eq!(config.triggers.len(), 1);
        assert_eq!(config.triggers[0].path, "/");
        assert_eq!(config.triggers[0].method, "POST");
        assert_eq!(config.triggers[0].cron.as_str(), "@daily");
        let config = parse_app(
            "[[triggers]]\ncron = \"*/5 * * * *\"\npath = \"/cron/x\"\nmethod = \"GET\"\n",
            "a",
        )
        .unwrap();
        assert_eq!(config.triggers[0].path, "/cron/x");
        assert_eq!(config.triggers[0].method, "GET");
    }

    #[test]
    fn a_schedule_that_does_not_parse_refuses_the_app_naming_the_key() {
        let why = parse_app("[[triggers]]\ncron = \"0 0 * *\"\n", "a").unwrap_err();
        assert!(why.contains("`[[triggers]] cron`"), "{why}");
        assert!(
            why.contains("5"),
            "it says how many fields a schedule has: {why}"
        );
    }

    #[test]
    fn an_unknown_triggers_key_is_refused_by_name() {
        let why = parse_app("[[triggers]]\ncron = \"@daily\"\nwhen = \"noon\"\n", "a").unwrap_err();
        assert!(why.contains("when"), "{why}");
    }

    #[test]
    fn a_trigger_path_or_method_that_could_not_be_a_request_is_refused() {
        for bad in [
            "[[triggers]]\ncron = \"@daily\"\npath = \"cron\"\n",
            "[[triggers]]\ncron = \"@daily\"\npath = \"/a b\"\n",
            "[[triggers]]\ncron = \"@daily\"\npath = \"/a?b=1\"\n",
        ] {
            let why = parse_app(bad, "a").unwrap_err();
            assert!(why.contains("is not a path"), "{why}");
        }
        let why =
            parse_app("[[triggers]]\ncron = \"@daily\"\nmethod = \"post\"\n", "a").unwrap_err();
        assert!(why.contains("is not a method"), "{why}");
    }

    #[test]
    fn more_schedules_than_an_app_may_have_are_refused() {
        let one = "[[triggers]]\ncron = \"@daily\"\n";
        assert!(parse_app(&one.repeat(MAX_TRIGGERS), "a").is_ok());
        let why = parse_app(&one.repeat(MAX_TRIGGERS + 1), "a").unwrap_err();
        assert!(why.contains(&MAX_TRIGGERS.to_string()), "{why}");
    }

    #[test]
    fn an_app_without_an_assets_section_gets_the_default_policy() {
        let config = parse_app("", "a").unwrap();
        assert_eq!(config.assets, AssetPolicy::default());
        assert_eq!(config.assets.max_bytes, 8 << 20);
        assert_eq!(config.assets.cache_control, "no-cache");
    }

    #[test]
    fn the_assets_size_and_cache_control_are_read() {
        let config = parse_app(
            "[assets]\nmax_bytes = 1048576\ncache_control = \"public, max-age=31536000, immutable\"\n",
            "a",
        )
        .unwrap();
        assert_eq!(config.assets.max_bytes, 1 << 20);
        assert_eq!(
            config.assets.cache_control,
            "public, max-age=31536000, immutable"
        );
    }

    #[test]
    fn an_unknown_assets_key_is_refused_by_name() {
        let error = parse_app("[assets]\nmaxbytes = 1024\n", "a").unwrap_err();
        assert!(error.contains("maxbytes"), "{error}");
    }

    #[test]
    fn an_assets_size_of_zero_or_above_what_a_deploy_can_carry_is_refused() {
        let error = parse_app("[assets]\nmax_bytes = 0\n", "a").unwrap_err();
        assert!(error.contains("`[assets] max_bytes` is 0"), "{error}");
        let error = parse_app("[assets]\nmax_bytes = 16777217\n", "a").unwrap_err();
        assert!(error.contains("could never be used"), "{error}");
        // The ceiling itself is a size like any other.
        parse_app("[assets]\nmax_bytes = 16777216\n", "a").unwrap();
    }

    #[test]
    fn a_cache_control_that_cannot_be_sent_is_refused() {
        let error = parse_app("[assets]\ncache_control = \"no-cache\\nx: y\"\n", "a").unwrap_err();
        assert!(error.contains("cannot be sent"), "{error}");
        let error = parse_app("[assets]\ncache_control = \"\"\n", "a").unwrap_err();
        assert!(error.contains("cache_control"), "{error}");
    }

    #[test]
    fn a_heap_above_the_ceiling_is_refused() {
        let error = parse_app("[limits]\nmax_heap_words = 9999999999\n", "a").unwrap_err();
        assert!(error.contains("largest heap a run can have"), "{error}");
        // Above the runtime's default heap is a capacity like any other now.
        parse_app("[limits]\nmax_heap_words = 99999999\n", "a").unwrap();
    }

    #[test]
    fn allowlist_entries() {
        let rule = AllowRule::parse("https://API.example.com").unwrap();
        assert!(rule.admits("https", "api.example.com", 443));
        assert!(!rule.admits("http", "api.example.com", 443));
        assert!(!rule.admits("https", "api.example.com", 8443));
        let any = AllowRule::parse("http://127.0.0.1:*").unwrap();
        assert!(any.admits("http", "127.0.0.1", 9999));
        assert!(!any.admits("http", "127.0.0.2", 9999));
        assert!(AllowRule::parse("ftp://x").is_err());
        assert!(AllowRule::parse("http://x/path").is_err());
        assert!(AllowRule::parse("example.com").is_err());
        let config = parse_app("[fetch]\nallow = [\"http://localhost:8080\"]\n", "a").unwrap();
        assert!(config.fetch.admits("http", "localhost", 8080));
    }

    #[test]
    fn secrets_resolve_and_never_print() {
        std::env::set_var("COVE_HOST_TEST_SECRET", "from-env");
        let config = parse_app(
            "[secrets]\na = { value = \"literal\" }\nb = { env = \"COVE_HOST_TEST_SECRET\" }\n",
            "x",
        )
        .unwrap();
        assert_eq!(config.secrets.0["a"], "literal");
        assert_eq!(config.secrets.0["b"], "from-env");
        assert_eq!(format!("{:?}", config.secrets), "{\"a\", \"b\"}");
        let missing = parse_app(
            "[secrets]\nc = { env = \"COVE_HOST_NOT_SET_ANYWHERE\" }\n",
            "x",
        );
        assert!(missing.unwrap_err().contains("is not set"));
        assert!(parse_app("[secrets]\nd = { value = \"v\", env = \"E\" }\n", "x").is_err());
    }

    #[test]
    fn a_store_secret_comes_from_the_store_or_refuses_the_app_by_name() {
        let toml = "[secrets]\ngemini = { store = \"gemini-key\" }\n";
        // No store at all, and a store without it: refused, naming both.
        let error = parse_app(toml, "x").unwrap_err();
        assert!(
            error.contains("secret `gemini`") && error.contains("`--data`"),
            "{error}"
        );
        let store = Arc::new(SecretStore::in_memory());
        let source = SecretSource::store(Arc::clone(&store));
        let error =
            parse_app_with(toml, "x", None, None, &source, RemovedKeys::Refuse).unwrap_err();
        assert!(error.contains("has no `gemini-key`"), "{error}");
        store.set("gemini-key", "sk-from-the-store").unwrap();
        let config = parse_app_with(toml, "x", None, None, &source, RemovedKeys::Refuse).unwrap();
        assert_eq!(config.secrets.0["gemini"], "sk-from-the-store");
        assert!(config.placeholders.is_empty());
        // One source only, a name the store could hold, and secrets only.
        assert!(parse_app("[secrets]\ng = { store = \"a\", env = \"E\" }\n", "x").is_err());
        assert!(parse_app("[secrets]\ng = { store = \"a b\" }\n", "x")
            .unwrap_err()
            .contains("not a secret name"));
        assert!(parse_app("[access]\nteam = { store = \"t\" }\n", "x")
            .unwrap_err()
            .contains("cannot come from the secret store"));
        // What `store_references` reads.
        let dir = std::env::temp_dir().join(format!("minicloud-store-refs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("app.toml"),
            format!("{toml}other = {{ env = \"E\" }}\nthird = {{ store = \"b\" }}\n"),
        )
        .unwrap();
        let names: Vec<String> = store_references(&dir).into_iter().collect();
        assert_eq!(names, ["b", "gemini-key"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn placeholders_stand_in_for_what_cannot_be_resolved() {
        let source = SecretSource {
            store: None,
            placeholders: true,
        };
        let config = parse_app_with(
            "[secrets]\na = { store = \"x\" }\nb = { env = \"COVE_HOST_NOT_SET_ANYWHERE\" }\n\
             c = { value = \"kept\" }\n",
            "x",
            None,
            None,
            &source,
            RemovedKeys::Refuse,
        )
        .unwrap();
        assert_eq!(config.placeholders, ["a", "b"]);
        assert_eq!(config.secrets.0["a"], placeholder("a"));
        assert_eq!(config.secrets.0["c"], "kept");
        // A malformed entry is still refused.
        assert!(parse_app_with(
            "[secrets]\na = {}\n",
            "x",
            None,
            None,
            &source,
            RemovedKeys::Refuse
        )
        .is_err());
    }

    const OPENAI: &str = "[secrets]\nopenai = { value = \"sk-not-a-real-key\" }\n\
                          gemini = { value = \"AIza-not-a-real-key\" }\n\
                          [fetch]\nallow = [\"https://api.openai.com\", \
                          \"https://generativelanguage.googleapis.com:443\", \"http://localhost:*\"]\n";

    #[test]
    fn secret_headers_are_read_and_never_print() {
        let config = parse_app(
            &format!(
                "{OPENAI}[fetch.headers.\"https://api.openai.com\"]\n\
                 Authorization = {{ secret = \"openai\", prefix = \"Bearer \" }}\n\
                 [fetch.headers.\"https://generativelanguage.googleapis.com\"]\n\
                 x-goog-api-key = {{ secret = \"gemini\" }}\n"
            ),
            "x",
        )
        .unwrap();
        let headers = &config.fetch.headers;
        assert_eq!(headers.len(), 2);
        let openai = &headers[0];
        assert_eq!(openai.name.as_str(), "authorization");
        assert_eq!(openai.value.to_str().unwrap(), "Bearer sk-not-a-real-key");
        assert!(openai.value.is_sensitive());
        assert!(openai.applies_to("https", "api.openai.com", 443));
        assert!(!openai.applies_to("https", "api.openai.com", 8443));
        assert!(!openai.applies_to("http", "api.openai.com", 443));
        assert!(!openai.applies_to("https", "evil.openai.com", 443));
        // The default port written out matches the entry written without it.
        let gemini = &headers[1];
        assert_eq!(gemini.value.to_str().unwrap(), "AIza-not-a-real-key");
        assert!(gemini.applies_to("https", "generativelanguage.googleapis.com", 443));
        let printed = format!("{config:?}");
        assert!(!printed.contains("not-a-real-key"), "{printed}");
        assert!(printed.contains("<secret `openai`>"), "{printed}");
    }

    #[test]
    fn secret_headers_are_held_to_their_rules() {
        for (headers, wanted) in [
            (
                "[fetch.headers.\"https://api.example.com\"]\nx-key = { secret = \"openai\" }\n",
                "is not an entry of `[fetch] allow`",
            ),
            (
                "[fetch.headers.\"https://api.openai.com:8443\"]\nx-key = { secret = \"openai\" }\n",
                "is not an entry of `[fetch] allow`",
            ),
            (
                "[fetch.headers.\"http://localhost:*\"]\nx-key = { secret = \"openai\" }\n",
                "is not an origin",
            ),
            (
                "[fetch.headers.\"https://api.openai.com/v1\"]\nx-key = { secret = \"openai\" }\n",
                "is not an origin",
            ),
            (
                "[fetch.headers.\"https://api.openai.com\"]\nx-key = { secret = \"nope\" }\n",
                "names no `[secrets]` entry",
            ),
            (
                "[fetch.headers.\"https://api.openai.com\"]\n\"x key\" = { secret = \"openai\" }\n",
                "is not a header name",
            ),
            (
                "[fetch.headers.\"https://api.openai.com\"]\n\
                 x-key = { secret = \"openai\", prefix = \"a\\nb\" }\n",
                "`prefix`",
            ),
            (
                "[fetch.headers.\"https://api.openai.com\"]\nx-key = { env = \"OPENAI\" }\n",
                "env",
            ),
            (
                "[fetch.headers.\"https://api.openai.com\"]\nx-key = \"literal\"\n",
                "",
            ),
        ] {
            let error = parse_app(&format!("{OPENAI}{headers}"), "x").unwrap_err();
            assert!(error.contains(wanted), "{headers}: {error}");
            assert!(!error.contains("not-a-real-key"), "{headers}: {error}");
        }
        // A secret that cannot be a header value is refused without quoting it.
        let error = parse_app(
            "[secrets]\nbad = { value = \"line\\nbreak-not-a-real-key\" }\n\
             [fetch]\nallow = [\"https://api.openai.com\"]\n\
             [fetch.headers.\"https://api.openai.com\"]\nx-key = { secret = \"bad\" }\n",
            "x",
        )
        .unwrap_err();
        assert!(error.contains("secret `bad` cannot be sent"), "{error}");
        assert!(!error.contains("not-a-real-key"), "{error}");
    }

    #[test]
    fn an_override_cannot_move_a_secret_header() {
        let text = format!(
            "{OPENAI}[fetch.headers.\"https://api.openai.com\"]\n\
             authorization = {{ secret = \"openai\", prefix = \"Bearer \" }}\n"
        );
        // Taking the origin off the allowlist loads: requests there are
        // refused, and the header goes nowhere else.
        let removed = AppOverride {
            fetch_allow_remove: vec!["https://api.openai.com".to_string()],
            fetch_allow_add: vec!["https://api.example.com".to_string()],
            ..AppOverride::none()
        };
        let config = parse_app_with(
            &text,
            "x",
            None,
            Some(&removed),
            &SecretSource::default(),
            RemovedKeys::Refuse,
        )
        .unwrap();
        assert!(!config.fetch.admits("https", "api.openai.com", 443));
        assert!(config.fetch.admits("https", "api.example.com", 443));
        assert!(config.fetch.headers.iter().all(|header| !header.applies_to(
            "https",
            "api.example.com",
            443
        )));
        // An origin only an override allows cannot be given a secret header.
        let added = AppOverride {
            fetch_allow_add: vec!["https://api.example.com".to_string()],
            ..AppOverride::none()
        };
        let error = parse_app_with(
            &format!(
                "{OPENAI}[fetch.headers.\"https://api.example.com\"]\n\
                 x-key = {{ secret = \"openai\" }}\n"
            ),
            "x",
            None,
            Some(&added),
            &SecretSource::default(),
            RemovedKeys::Refuse,
        )
        .unwrap_err();
        assert!(
            error.contains("is not an entry of `[fetch] allow`"),
            "{error}"
        );
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("5s").unwrap(), Duration::from_secs(5));
        assert_eq!(parse_duration("250ms").unwrap(), Duration::from_millis(250));
        // Minutes and hours, for `[rate] per` (#54).
        assert_eq!(parse_duration("5m").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_secs(7200));
        // `ms` is milliseconds, never "a number of m, then s".
        assert_eq!(parse_duration("1ms").unwrap(), Duration::from_millis(1));
        assert!(parse_duration("5 weeks").is_err());
        assert!(parse_duration("5").is_err());
    }

    #[test]
    fn an_override_is_kept_as_what_changed_from_the_file() {
        let file = parse_app(
            "grant = [\"kv\", \"log\"]\n[limits]\nmax_host_calls = 10\n[fetch]\nallow = [\"https://a.example\"]\n",
            "notes",
        )
        .unwrap();
        let grant: BTreeSet<String> = ["log", "time"].iter().map(|s| s.to_string()).collect();
        let mut limits = file.limits.as_file();
        limits.max_host_calls = Some(20);
        let over = AppOverride::between(
            &file,
            &grant,
            &["https://b.example".to_string()],
            &limits,
            true,
        );
        assert_eq!(over.grant_add, ["time"]);
        assert_eq!(over.grant_remove, ["kv"]);
        assert_eq!(over.fetch_allow_add, ["https://b.example"]);
        assert_eq!(over.fetch_allow_remove, ["https://a.example"]);
        // Only the limit that differs from what the file comes to.
        assert_eq!(
            over.limits,
            LimitsFile {
                max_host_calls: Some(20),
                ..LimitsFile::default()
            }
        );
        // Applied to a later file that grants something new, the new grant
        // stays: only what was taken away is.
        let later = parse_app_with(
            "grant = [\"kv\", \"log\", \"random\"]\n",
            "notes",
            None,
            Some(&over),
            &SecretSource::default(),
            RemovedKeys::Refuse,
        )
        .unwrap();
        let granted: Vec<&str> = later.granted.iter().map(String::as_str).collect();
        assert_eq!(granted, ["log", "random", "time"]);
        assert_eq!(later.limits.run.max_host_calls, Some(20));
        assert_eq!(later.file_allow, ["https://b.example"]);
        // Nothing changed is no override.
        let same = AppOverride::between(
            &file,
            &file.granted,
            &file.file_allow,
            &file.limits.as_file(),
            true,
        );
        assert!(same.is_empty(), "{same:?}");
    }

    #[test]
    fn admin_and_hostnames_are_held_to_their_rules() {
        let error = parse_app("grant = [\"admin\"]\n", "notes").unwrap_err();
        assert!(error.contains("only the app `admin`"), "{error}");
        assert!(parse_app("grant = [\"admin\"]\n", "admin").is_ok());
        let config = parse_app("[route]\nhosts = [\"Admin.Example\"]\n", "admin").unwrap();
        assert_eq!(config.hosts, ["admin.example"]);
        for bad in ["https://a.example", "a.example:80", "a..example", ""] {
            let text = format!("[route]\nhosts = [\"{bad}\"]\n");
            assert!(parse_app(&text, "a").is_err(), "{bad}");
        }
    }

    #[test]
    fn access_is_on_with_a_team_and_an_aud_and_off_without() {
        let secret = "[secrets]\nadmin = { value = \"s\" }\n";
        let on = parse_app(
            &format!(
                "{secret}[access]\nteam = {{ value = \"t.cloudflareaccess.com\" }}\n\
                 aud = {{ value = \"a1, a2\" }}\nemails = {{ value = \"A@x.com b@y.com\" }}\n\
                 token = \"admin\"\n"
            ),
            "x",
        )
        .unwrap();
        let policy = on.access.jwt.unwrap();
        assert_eq!(policy.issuer, "https://t.cloudflareaccess.com");
        assert_eq!(policy.audiences, ["a1", "a2"]);
        assert_eq!(policy.emails, ["a@x.com", "b@y.com"]);
        assert!(!on.access.fallback);
        assert_eq!(on.access.token.as_deref(), Some("admin"));
        // An unset environment variable turns it off, and says so.
        let off = parse_app(
            &format!(
                "{secret}[access]\nteam = {{ env = \"COVE_HOST_NOT_SET_ANYWHERE\" }}\n\
                 aud = {{ value = \"a\" }}\ntoken = \"admin\"\n"
            ),
            "x",
        )
        .unwrap();
        assert!(off.access.jwt.is_none());
        assert!(off.access.off.is_some());
        // No `[access]` at all: off, and nothing to say.
        let none = parse_app(secret, "x").unwrap();
        assert!(none.access.jwt.is_none() && none.access.off.is_none());
    }

    #[test]
    fn access_is_held_to_its_rules() {
        let secret = "[secrets]\nadmin = { value = \"s\" }\n";
        for (access, wanted) in [
            ("token = \"nope\"\n", "names no `[secrets]` entry"),
            ("fallback = \"maybe\"\n", "is not \"none\" or \"token\""),
            ("fallback = \"token\"\n", "needs `access.token`"),
            (
                "team = { value = \"t.example/x\" }\naud = { value = \"a\" }\n",
                "team domain",
            ),
            ("aud = { value = \"a\", env = \"B\" }\n", "exactly one of"),
            ("audience = \"a\"\n", "audience"),
        ] {
            let error = parse_app(&format!("{secret}[access]\n{access}"), "x").unwrap_err();
            assert!(error.contains(wanted), "{access}: {error}");
        }
        let fallback = parse_app(
            &format!("{secret}[access]\ntoken = \"admin\"\nfallback = \"token\"\n"),
            "x",
        )
        .unwrap();
        assert!(fallback.access.fallback);
    }
}
