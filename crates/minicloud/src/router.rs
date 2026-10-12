//! Which app a request is for.
//!
//! A [`Router`] is asked with the request's `Host` header and its path, and
//! answers the app's name and the path the app sees; the host then looks the
//! name up among the apps it serves now, so an app added or removed by an
//! update is routed to, or not, without the router knowing. [`PathPrefix`] is
//! the one this host uses: `/<app>/rest` reaches `<app>` with `/rest`.
//! [`ByHostname`] puts hostnames in front of it: an app whose `app.toml` has
//! `[route] hosts = ["admin.example"]` is reached by `Host: admin.example`
//! with the whole path, and by nothing else — `/<app>/` on any other
//! hostname is not it, so a hostname with its own access policy in front
//! (the admin app's) cannot be reached through one with another.

use std::collections::{BTreeMap, BTreeSet};

/// Where a request goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Route {
    /// The app's name.
    pub app: String,
    /// The path the app sees; starts with `/`.
    pub path: String,
    /// The hostname the request was routed by, for an app reached by
    /// hostname: its public origin is that name's
    /// ([`crate::proxy::Forwarding::apply`]).
    pub hostname: Option<String>,
}

/// Chooses the app for a request.
pub trait Router: Send + Sync {
    /// The app `path` (and `host`, the `Host` header without its port, when
    /// the request had one) is for, or `None` for no app.
    fn route(&self, host: Option<&str>, path: &str) -> Option<Route>;
}

/// `/<app>/rest` reaches `<app>` with `/rest`; `/<app>` reaches it with `/`.
pub struct PathPrefix;

impl Router for PathPrefix {
    fn route(&self, _host: Option<&str>, path: &str) -> Option<Route> {
        let path = path.strip_prefix('/')?;
        let (name, rest) = match path.split_once('/') {
            Some((name, rest)) => (name, format!("/{rest}")),
            None => (path, "/".to_string()),
        };
        (!name.is_empty()).then(|| Route {
            app: name.to_string(),
            path: rest,
            hostname: None,
        })
    }
}

/// Hostnames first, then [`PathPrefix`] for every app that has none.
pub struct ByHostname {
    /// Hostname (lower case, no port) to app name.
    pub hosts: std::sync::Arc<BTreeMap<String, String>>,
}

impl ByHostname {
    /// The apps reached by hostname only.
    fn hosted(&self) -> BTreeSet<&str> {
        self.hosts.values().map(String::as_str).collect()
    }
}

impl ByHostname {
    /// The app a hostname reaches: the name itself, or a `*.` pattern one
    /// label below it.
    ///
    /// One label, never more: `*.example` is `a.example` and not
    /// `a.b.example`, because a wildcard that crossed a dot would claim a
    /// name whose own `*.b.example` is somebody else's.
    fn app_for(&self, host: &str) -> Option<&String> {
        if let Some(app) = self.hosts.get(host) {
            return Some(app);
        }
        let (label, rest) = host.split_once('.')?;
        if label.is_empty() || rest.is_empty() {
            return None;
        }
        self.hosts.get(&format!("*.{rest}"))
    }
}

impl Router for ByHostname {
    fn route(&self, host: Option<&str>, path: &str) -> Option<Route> {
        if let Some(host) = host {
            let host = host.trim_end_matches('.').to_ascii_lowercase();
            if let Some(app) = self.app_for(&host) {
                return Some(Route {
                    app: app.clone(),
                    path: if path.is_empty() {
                        "/".to_string()
                    } else {
                        path.to_string()
                    },
                    hostname: Some(host),
                });
            }
        }
        PathPrefix
            .route(host, path)
            .filter(|route| !self.hosted().contains(route.app.as_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_prefix_routes_to_its_app_with_the_rest_of_the_path() {
        let route = |path| PathPrefix.route(None, path);
        assert_eq!(
            route("/crunch/a/b"),
            Some(Route {
                app: "crunch".to_string(),
                path: "/a/b".to_string(),
                hostname: None,
            })
        );
        assert_eq!(route("/hello").unwrap().path, "/");
        assert_eq!(route("/hello/").unwrap().path, "/");
        assert_eq!(route("/"), None);
    }

    #[test]
    fn a_hostname_reaches_its_app_only_by_that_name() {
        let router = ByHostname {
            hosts: std::sync::Arc::new([("admin.example".to_string(), "admin".to_string())].into()),
        };
        let route = router.route(Some("Admin.Example"), "/apps/x").unwrap();
        assert_eq!(route.app, "admin");
        assert_eq!(route.path, "/apps/x");
        assert_eq!(route.hostname.as_deref(), Some("admin.example"));
        assert_eq!(router.route(Some("admin.example"), "/").unwrap().path, "/");
        // The whole path, prefix or not: the hostname is the app's.
        assert_eq!(
            router.route(Some("admin.example"), "/hello/x").unwrap().app,
            "admin"
        );
        // Not by its prefix on another name.
        assert_eq!(router.route(Some("tools.example"), "/admin/apps"), None);
        assert_eq!(router.route(None, "/admin/"), None);
        // Other apps as before.
        let other = router.route(Some("tools.example"), "/hello/x").unwrap();
        assert_eq!((other.app.as_str(), other.path.as_str()), ("hello", "/x"));
        assert_eq!(other.hostname, None);
    }
}
