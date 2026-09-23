//! Server settings.
//!
//! Environment variable names are deliberately unchanged from the
//! shell/Node implementation this replaces, so no machine and no cloud
//! environment needs re-provisioning to switch over.

use std::env;
use std::time::Duration;

/// Why the server cannot start.
#[derive(Debug, Clone, thiserror::Error)]
pub enum ConfigError {
    /// Mirrors the Node server's refusal to start without auth — a server
    /// reachable from the internet with no token is not a degraded mode
    /// worth supporting.
    #[error("RECALL_TOKEN is not set; refusing to start with no auth")]
    MissingToken,
    /// `RECALL_TLS_CERT` and `RECALL_TLS_KEY` name one file each; half a
    /// pair is almost always a typo in one of the two variable names.
    #[error("RECALL_TLS_CERT and RECALL_TLS_KEY must both be set, or neither")]
    PartialTlsFiles,
    /// `RECALL_TLS_ACME_DOMAINS` and `RECALL_TLS_ACME_EMAIL` are the same
    /// kind of pair, for the same reason.
    #[error("RECALL_TLS_ACME_DOMAINS and RECALL_TLS_ACME_EMAIL must both be set, or neither")]
    PartialTlsAcme,
    /// The two TLS modes are mutually exclusive: each picks its own
    /// certificate source, and a config with both is ambiguous about which
    /// one wins rather than a config this server can just run.
    #[error(
        "RECALL_TLS_CERT/RECALL_TLS_KEY and RECALL_TLS_ACME_DOMAINS/RECALL_TLS_ACME_EMAIL \
         are two different TLS modes; set one, not both"
    )]
    BothTlsModes,
    /// With direct TLS there is no ingress, so the socket's own peer
    /// address is the only client IP that is not the client's own choice.
    /// Setting this variable anyway is either a no-op or, if it is ever
    /// read, a way for a client to buy itself an unlimited number of token
    /// guesses by rotating whatever header it names, so refusing to start
    /// beats silently ignoring the setting.
    #[error(
        "RECALL_TRUSTED_IP_HEADER must not be set while TLS is on; with direct TLS there is no \
         ingress, so the client IP always comes from the socket's peer address"
    )]
    TrustedIpHeaderWithTls,
}

/// What `recall-server` needs.
///
/// Every field has a default that is safe to run with, except [`token`],
/// which has none — see [`ConfigError::MissingToken`].
///
/// | Field | Variable | Default |
/// |---|---|---|
/// | [`addr`] | `RECALL_HOST`, `RECALL_PORT` | `0.0.0.0:8787` |
/// | [`token`] | `RECALL_TOKEN` | *required* |
/// | [`db_path`] | `RECALL_DB_PATH` | `data/recall.db` |
/// | [`git_commit`] | `RECALL_GIT_COMMIT` | the commit the binary was built from, else `unknown` |
/// | [`backup_dir`] | `RECALL_BACKUP_DIR` | off |
/// | [`backup_interval`] | `RECALL_BACKUP_INTERVAL_MS` | 24h |
/// | [`backup_keep`] | `RECALL_BACKUP_KEEP` | 7 |
/// | [`rate_limit_window`] | `RECALL_RATE_LIMIT_WINDOW_MS` | 60s |
/// | [`rate_limit_max`] | `RECALL_RATE_LIMIT_MAX` | 60 |
/// | [`trusted_ip_header`] | `RECALL_TRUSTED_IP_HEADER` | `cf-connecting-ip`, forced empty when [`tls`] is on |
/// | [`merge_enabled`] | `RECALL_MERGE_ENABLED` | on |
/// | [`merge_timeout`] | `RECALL_MERGE_TIMEOUT_MS` | 45s |
/// | [`claude_bin`] | `RECALL_CLAUDE_BIN` | `claude` |
/// | [`claude_status_interval`] | `RECALL_CLAUDE_STATUS_INTERVAL_MS` | 30m |
/// | [`tls`] | `RECALL_TLS_CERT`/`RECALL_TLS_KEY`, or `RECALL_TLS_ACME_DOMAINS`/`RECALL_TLS_ACME_EMAIL`/`RECALL_TLS_ACME_DIR`/`RECALL_TLS_ACME_STAGING` | off |
///
/// [`addr`]: Config::addr
/// [`token`]: Config::token
/// [`db_path`]: Config::db_path
/// [`git_commit`]: Config::git_commit
/// [`backup_dir`]: Config::backup_dir
/// [`backup_interval`]: Config::backup_interval
/// [`backup_keep`]: Config::backup_keep
/// [`rate_limit_window`]: Config::rate_limit_window
/// [`rate_limit_max`]: Config::rate_limit_max
/// [`trusted_ip_header`]: Config::trusted_ip_header
/// [`merge_enabled`]: Config::merge_enabled
/// [`merge_timeout`]: Config::merge_timeout
/// [`claude_bin`]: Config::claude_bin
/// [`claude_status_interval`]: Config::claude_status_interval
/// [`tls`]: Config::tls
#[derive(Debug, Clone)]
pub struct Config {
    /// The socket to bind, assembled from host and port.
    pub addr: String,
    /// The single bearer token. There is no second one, by design.
    pub token: String,
    /// The SQLite file. Opened, never created from a schema migration — it
    /// is the same file the Node server wrote.
    pub db_path: String,
    /// Reported by `GET /health` so a deploy can be confirmed from outside.
    /// A release binary knows its own commit, stamped at build time; the
    /// variable overrides it, for an image built from a checkout.
    pub git_commit: String,

    /// Where periodic database snapshots go. Empty disables backups.
    pub backup_dir: String,
    /// How often to take one.
    pub backup_interval: Duration,
    /// How many to keep before deleting the oldest.
    pub backup_keep: usize,

    /// The window rate limiting counts requests over.
    pub rate_limit_window: Duration,
    /// How many requests one client may make in that window.
    pub rate_limit_max: u32,
    /// The one request header whose value is taken as the client's address,
    /// or empty to trust none and use the socket's peer address.
    ///
    /// Rate limiting keys off this, and rate limiting runs *before* auth
    /// precisely so a flood of invalid tokens is limited too — so a client
    /// that can choose its own value here can rotate it and get unlimited
    /// attempts at guessing the token.
    ///
    /// That makes this a statement about the deployment, not a preference:
    /// it names the header the *ingress* sets, and it is only safe when
    /// nothing can reach this server except through that ingress. Exactly
    /// one header is read, so a value the client supplies under any other
    /// name is ignored.
    ///
    /// | Ingress | Set this to |
    /// |---|---|
    /// | Cloudflare Tunnel | `cf-connecting-ip` (the default) |
    /// | Traefik, nginx, Caddy | `x-real-ip` |
    /// | None — reached directly | empty |
    ///
    /// Deliberately *not* `x-forwarded-for`: a proxy appends to it, so the
    /// first entry is whatever the client sent. Reading it as one value is
    /// the classic way to make this setting useless.
    pub trusted_ip_header: String,

    /// Whether to attempt semantic merge at all. Off means last-write-wins.
    pub merge_enabled: bool,
    /// How long a merge may take before it is abandoned — and, like every
    /// other merge failure, degraded to last-write-wins.
    pub merge_timeout: Duration,
    /// The `claude` binary to shell out to. Never the Anthropic API.
    pub claude_bin: String,
    /// How often to re-check that the binary is present and logged in.
    pub claude_status_interval: Duration,

    /// Whether this server terminates TLS itself. Off by default: the two
    /// existing deployments (`deploy/docker-compose.yml`,
    /// `docker-compose.traefik.yml`) put an ingress in front instead, and
    /// that stays the default. See [`TlsMode`].
    pub tls: TlsMode,
}

/// Whether, and how, `recall-server` terminates TLS itself rather than
/// leaving it to an ingress.
///
/// The two modes are mutually exclusive and each requires its own pair of
/// variables in full; see [`ConfigError::PartialTlsFiles`],
/// [`ConfigError::PartialTlsAcme`] and [`ConfigError::BothTlsModes`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TlsMode {
    /// Plain HTTP on [`Config::addr`]. An ingress is expected to terminate
    /// TLS in front of this, per `deploy/README.md`.
    Off,
    /// `RECALL_TLS_CERT` + `RECALL_TLS_KEY`: serve HTTPS on
    /// [`Config::addr`] from a certificate and key already on disk, such as
    /// one a separate ACME client keeps renewed.
    Files {
        /// PEM certificate chain path (`RECALL_TLS_CERT`).
        cert_path: String,
        /// PEM private key path (`RECALL_TLS_KEY`).
        key_path: String,
    },
    /// `RECALL_TLS_ACME_DOMAINS` + `RECALL_TLS_ACME_EMAIL`: the server gets
    /// and renews its own certificate from an ACME directory (Let's
    /// Encrypt, by default) over TLS-ALPN-01, which needs only the one port
    /// it is already serving on, port 80 is never touched.
    Acme {
        /// The domain names to request a certificate for.
        domains: Vec<String>,
        /// The contact address the ACME directory may use for expiry
        /// notices.
        email: String,
        /// Where the issued certificate and account key are cached, so a
        /// restart does not re-issue one (`RECALL_TLS_ACME_DIR`).
        cache_dir: String,
        /// Let's Encrypt's staging directory instead of production
        /// (`RECALL_TLS_ACME_STAGING`): much higher rate limits while
        /// testing, at the cost of a certificate no client will trust.
        staging: bool,
    },
}

impl TlsMode {
    /// Whether the server terminates TLS at all, in either mode.
    pub fn is_enabled(&self) -> bool {
        !matches!(self, TlsMode::Off)
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            addr: "0.0.0.0:8787".to_string(),
            token: String::new(),
            db_path: "data/recall.db".to_string(),
            git_commit: "unknown".to_string(),
            backup_dir: String::new(),
            backup_interval: Duration::from_secs(24 * 60 * 60),
            backup_keep: 7,
            rate_limit_window: Duration::from_secs(60),
            rate_limit_max: 60,
            trusted_ip_header: "cf-connecting-ip".to_string(),
            merge_enabled: true,
            merge_timeout: Duration::from_secs(45),
            claude_bin: "claude".to_string(),
            claude_status_interval: Duration::from_secs(30 * 60),
            tls: TlsMode::Off,
        }
    }
}

impl Config {
    /// Reads configuration from the real process environment.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|key| env::var(key).ok())
    }

    /// Reads configuration through a caller-supplied lookup, applying the
    /// same defaults the Node implementation used.
    ///
    /// The lookup is injected rather than read from `std::env` inside so the
    /// clamping below is testable: `set_var` is process-global, and Rust runs
    /// tests in parallel threads, so an env-reading test races every other
    /// test in the binary.
    ///
    /// An unparseable value falls back rather than failing the boot, matching
    /// the Node and Go implementations — a typo in one tunable should not be
    /// the reason a server won't start.
    pub fn from_lookup<F>(lookup: F) -> Result<Self, ConfigError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let get = |key: &str| lookup(key).filter(|v| !v.is_empty());
        let or = |key: &str, fallback: &str| get(key).unwrap_or_else(|| fallback.to_string());
        let num =
            |key: &str, fallback: u64| get(key).and_then(|v| v.parse().ok()).unwrap_or(fallback);

        let files_tls = match (get("RECALL_TLS_CERT"), get("RECALL_TLS_KEY")) {
            (Some(cert_path), Some(key_path)) => Some(TlsMode::Files {
                cert_path,
                key_path,
            }),
            (None, None) => None,
            _ => return Err(ConfigError::PartialTlsFiles),
        };
        let acme_tls = match (get("RECALL_TLS_ACME_DOMAINS"), get("RECALL_TLS_ACME_EMAIL")) {
            (Some(domains), Some(email)) => Some(TlsMode::Acme {
                domains: domains
                    .split(',')
                    .map(str::trim)
                    .filter(|d| !d.is_empty())
                    .map(str::to_string)
                    .collect(),
                email,
                cache_dir: or("RECALL_TLS_ACME_DIR", "/data/acme"),
                staging: get("RECALL_TLS_ACME_STAGING").as_deref() == Some("true"),
            }),
            (None, None) => None,
            _ => return Err(ConfigError::PartialTlsAcme),
        };
        let tls = match (files_tls, acme_tls) {
            (Some(_), Some(_)) => return Err(ConfigError::BothTlsModes),
            (Some(mode), None) | (None, Some(mode)) => mode,
            (None, None) => TlsMode::Off,
        };
        // With direct TLS there is no ingress, so the setting that protects
        // the rate limiter behind one is not just unnecessary but actively
        // dangerous here: reading it at all would let a direct client pick
        // its own rate-limit bucket by supplying whatever header it names.
        // Refusing to start beats silently ignoring a value left over from
        // moving a deployment from behind an ingress to direct TLS.
        if tls.is_enabled() && lookup("RECALL_TRUSTED_IP_HEADER").is_some() {
            return Err(ConfigError::TrustedIpHeaderWithTls);
        }

        let mut cfg = Config {
            addr: format!("0.0.0.0:{}", or("RECALL_PORT", "8787")),
            token: get("RECALL_TOKEN").unwrap_or_default(),
            db_path: or("RECALL_DB_PATH", "data/recall.db"),
            git_commit: or(
                "RECALL_GIT_COMMIT",
                option_env!("RECALL_GIT_COMMIT").unwrap_or("unknown"),
            ),
            backup_dir: get("RECALL_BACKUP_DIR").unwrap_or_default(),
            backup_interval: Duration::from_secs(
                num("RECALL_BACKUP_INTERVAL_HOURS", 24).saturating_mul(3600),
            ),
            backup_keep: num("RECALL_BACKUP_KEEP", 7) as usize,
            rate_limit_window: Duration::from_millis(num("RECALL_RATE_LIMIT_WINDOW_MS", 60_000)),
            rate_limit_max: num("RECALL_RATE_LIMIT_MAX", 60) as u32,
            // Lowercased because HeaderMap lookups are case-insensitive but
            // this is compared as a plain string. Forced empty under TLS
            // regardless of this default: the check above already refused
            // to start if the variable was set explicitly, and with no
            // ingress in front, no header is safe to trust at all.
            trusted_ip_header: if tls.is_enabled() {
                String::new()
            } else {
                lookup("RECALL_TRUSTED_IP_HEADER")
                    .map(|v| v.trim().to_ascii_lowercase())
                    .unwrap_or_else(|| "cf-connecting-ip".to_string())
            },
            // Opt-out, not opt-in: only the literal "false" disables it, so a
            // typo leaves merge on rather than silently off.
            merge_enabled: lookup("RECALL_MERGE_ENABLED").as_deref() != Some("false"),
            merge_timeout: Duration::from_millis(num("RECALL_MERGE_TIMEOUT_MS", 45_000)),
            claude_bin: or("RECALL_CLAUDE_BIN", "claude"),
            claude_status_interval: Duration::from_millis(num(
                "RECALL_CLAUDE_STATUS_INTERVAL_MS",
                30 * 60_000,
            )),
            tls,
        };
        if cfg.token.is_empty() {
            return Err(ConfigError::MissingToken);
        }
        // A zero interval would spin a background loop as fast as the
        // scheduler allows.
        if cfg.backup_interval.is_zero() {
            cfg.backup_interval = Duration::from_secs(24 * 60 * 60);
        }
        if cfg.rate_limit_window.is_zero() {
            cfg.rate_limit_window = Duration::from_secs(60);
        }
        if cfg.claude_status_interval.is_zero() {
            cfg.claude_status_interval = Duration::from_secs(30 * 60);
        }
        // A zero timeout is worse than a spinning loop: every merge would
        // hit an already-expired deadline and fail instantly, silently
        // degrading to last-write-wins with nothing in the logs that points
        // at the typo responsible.
        if cfg.merge_timeout.is_zero() {
            cfg.merge_timeout = Duration::from_millis(45_000);
        }
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| (*v).to_string())
        }
    }

    #[test]
    fn defaults_match_the_node_implementation() {
        let cfg = Config::default();
        assert_eq!(cfg.rate_limit_max, 60);
        assert_eq!(cfg.rate_limit_window, Duration::from_secs(60));
        assert_eq!(cfg.merge_timeout, Duration::from_secs(45));
        assert_eq!(cfg.backup_keep, 7);
        assert!(cfg.merge_enabled);
        assert_eq!(cfg.claude_bin, "claude");
    }

    #[test]
    fn refuses_to_start_without_a_token() {
        assert!(
            matches!(
                Config::from_lookup(env(&[])),
                Err(ConfigError::MissingToken)
            ),
            "a server reachable from the internet with no auth is not a degraded mode worth supporting"
        );
        // Set-but-empty is not set. Go's os.Getenv couldn't tell the two
        // apart; here it would otherwise boot with a token of "".
        assert!(matches!(
            Config::from_lookup(env(&[("RECALL_TOKEN", "")])),
            Err(ConfigError::MissingToken)
        ));
    }

    #[test]
    fn reads_every_override() {
        let cfg = Config::from_lookup(env(&[
            ("RECALL_TOKEN", "t"),
            ("RECALL_PORT", "9000"),
            ("RECALL_DB_PATH", "/data/x.db"),
            ("RECALL_GIT_COMMIT", "abc1234"),
            ("RECALL_BACKUP_DIR", "/backups"),
            ("RECALL_BACKUP_INTERVAL_HOURS", "6"),
            ("RECALL_BACKUP_KEEP", "3"),
            ("RECALL_RATE_LIMIT_WINDOW_MS", "1000"),
            ("RECALL_RATE_LIMIT_MAX", "5"),
            ("RECALL_MERGE_TIMEOUT_MS", "1234"),
            ("RECALL_CLAUDE_BIN", "/usr/bin/claude"),
            ("RECALL_CLAUDE_STATUS_INTERVAL_MS", "60000"),
        ]))
        .unwrap();

        assert_eq!(cfg.addr, "0.0.0.0:9000");
        assert_eq!(cfg.db_path, "/data/x.db");
        assert_eq!(cfg.git_commit, "abc1234");
        assert_eq!(cfg.backup_dir, "/backups");
        assert_eq!(cfg.backup_interval, Duration::from_secs(6 * 3600));
        assert_eq!(cfg.backup_keep, 3);
        assert_eq!(cfg.rate_limit_window, Duration::from_millis(1000));
        assert_eq!(cfg.rate_limit_max, 5);
        assert_eq!(cfg.merge_timeout, Duration::from_millis(1234));
        assert_eq!(cfg.claude_bin, "/usr/bin/claude");
        assert_eq!(cfg.claude_status_interval, Duration::from_millis(60_000));
    }

    #[test]
    fn merge_is_disabled_only_by_the_literal_false() {
        for (value, want) in [("false", false), ("true", true), ("0", true), ("", true)] {
            let cfg = Config::from_lookup(env(&[
                ("RECALL_TOKEN", "t"),
                ("RECALL_MERGE_ENABLED", value),
            ]))
            .unwrap();
            assert_eq!(cfg.merge_enabled, want, "RECALL_MERGE_ENABLED={value:?}");
        }
    }

    /// Every duration is clamped, not just the ones whose failure is loud.
    ///
    /// The Go implementation clamped only two of the four. A zero
    /// `CLAUDE_STATUS_INTERVAL` reached `time.NewTicker`, which panics on a
    /// non-positive duration — one config typo crashing the server at
    /// startup. A zero `MERGE_TIMEOUT` is quieter and worse: every merge
    /// hits an already-expired deadline and fails instantly, silently
    /// degrading to last-write-wins with nothing pointing at the cause.
    #[test]
    fn zero_and_unparseable_durations_fall_back_to_their_defaults() {
        for value in ["0", "not-a-number", "-5", " 6"] {
            let cfg = Config::from_lookup(env(&[
                ("RECALL_TOKEN", "t"),
                ("RECALL_BACKUP_INTERVAL_HOURS", value),
                ("RECALL_RATE_LIMIT_WINDOW_MS", value),
                ("RECALL_CLAUDE_STATUS_INTERVAL_MS", value),
                ("RECALL_MERGE_TIMEOUT_MS", value),
            ]))
            .unwrap();

            assert_eq!(
                cfg.backup_interval,
                Duration::from_secs(24 * 3600),
                "{value:?}"
            );
            assert_eq!(cfg.rate_limit_window, Duration::from_secs(60), "{value:?}");
            assert_eq!(
                cfg.claude_status_interval,
                Duration::from_secs(30 * 60),
                "{value:?}"
            );
            assert_eq!(
                cfg.merge_timeout,
                Duration::from_millis(45_000),
                "{value:?}"
            );
            assert!(
                !cfg.merge_timeout.is_zero(),
                "a zero merge timeout fails every merge instantly and silently"
            );
        }
    }

    #[test]
    fn tls_is_off_by_default() {
        assert_eq!(Config::default().tls, TlsMode::Off);
        let cfg = Config::from_lookup(env(&[("RECALL_TOKEN", "t")])).unwrap();
        assert_eq!(cfg.tls, TlsMode::Off);
        assert_eq!(cfg.trusted_ip_header, "cf-connecting-ip");
    }

    #[test]
    fn tls_files_mode_needs_both_variables() {
        for pairs in [
            &[("RECALL_TOKEN", "t"), ("RECALL_TLS_CERT", "/c.pem")][..],
            &[("RECALL_TOKEN", "t"), ("RECALL_TLS_KEY", "/k.pem")][..],
        ] {
            assert!(
                matches!(
                    Config::from_lookup(env(pairs)),
                    Err(ConfigError::PartialTlsFiles)
                ),
                "{pairs:?}"
            );
        }

        let cfg = Config::from_lookup(env(&[
            ("RECALL_TOKEN", "t"),
            ("RECALL_TLS_CERT", "/c.pem"),
            ("RECALL_TLS_KEY", "/k.pem"),
        ]))
        .unwrap();
        assert_eq!(
            cfg.tls,
            TlsMode::Files {
                cert_path: "/c.pem".to_string(),
                key_path: "/k.pem".to_string(),
            }
        );
    }

    #[test]
    fn tls_acme_mode_needs_both_variables_and_splits_domains() {
        for pairs in [
            &[
                ("RECALL_TOKEN", "t"),
                ("RECALL_TLS_ACME_DOMAINS", "example.com"),
            ][..],
            &[
                ("RECALL_TOKEN", "t"),
                ("RECALL_TLS_ACME_EMAIL", "me@example.com"),
            ][..],
        ] {
            assert!(
                matches!(
                    Config::from_lookup(env(pairs)),
                    Err(ConfigError::PartialTlsAcme)
                ),
                "{pairs:?}"
            );
        }

        let cfg = Config::from_lookup(env(&[
            ("RECALL_TOKEN", "t"),
            ("RECALL_TLS_ACME_DOMAINS", " a.example.com, b.example.com ,"),
            ("RECALL_TLS_ACME_EMAIL", "me@example.com"),
        ]))
        .unwrap();
        assert_eq!(
            cfg.tls,
            TlsMode::Acme {
                domains: vec!["a.example.com".to_string(), "b.example.com".to_string()],
                email: "me@example.com".to_string(),
                cache_dir: "/data/acme".to_string(),
                staging: false,
            }
        );
    }

    #[test]
    fn tls_acme_staging_and_cache_dir_are_overridable() {
        let cfg = Config::from_lookup(env(&[
            ("RECALL_TOKEN", "t"),
            ("RECALL_TLS_ACME_DOMAINS", "example.com"),
            ("RECALL_TLS_ACME_EMAIL", "me@example.com"),
            ("RECALL_TLS_ACME_DIR", "/tmp/acme-cache"),
            ("RECALL_TLS_ACME_STAGING", "true"),
        ]))
        .unwrap();
        let TlsMode::Acme {
            cache_dir, staging, ..
        } = cfg.tls
        else {
            panic!("expected TlsMode::Acme, got {:?}", cfg.tls);
        };
        assert_eq!(cache_dir, "/tmp/acme-cache");
        assert!(staging);
    }

    #[test]
    fn configuring_both_tls_modes_is_refused() {
        assert!(matches!(
            Config::from_lookup(env(&[
                ("RECALL_TOKEN", "t"),
                ("RECALL_TLS_CERT", "/c.pem"),
                ("RECALL_TLS_KEY", "/k.pem"),
                ("RECALL_TLS_ACME_DOMAINS", "example.com"),
                ("RECALL_TLS_ACME_EMAIL", "me@example.com"),
            ])),
            Err(ConfigError::BothTlsModes)
        ));
    }

    /// The mandatory security rule: behind an ingress the trusted header is
    /// how the rate limiter learns the real client address, but direct TLS
    /// has no ingress to set it, so a client that could still choose the
    /// value would buy itself unlimited token guesses. Setting the
    /// variable at all alongside TLS is refused outright, matching
    /// `scripts/trusted-ip-check.sh`'s socket-level proof of the same rule.
    #[test]
    fn trusted_ip_header_with_tls_refuses_to_start() {
        for tls_pairs in [
            &[("RECALL_TLS_CERT", "/c.pem"), ("RECALL_TLS_KEY", "/k.pem")][..],
            &[
                ("RECALL_TLS_ACME_DOMAINS", "example.com"),
                ("RECALL_TLS_ACME_EMAIL", "me@example.com"),
            ][..],
        ] {
            let mut pairs = vec![("RECALL_TOKEN", "t")];
            pairs.extend_from_slice(tls_pairs);

            // Even an explicit empty value, which would mean the same thing
            // TLS mode forces anyway, is refused: the point is that this
            // variable is not this deployment's business to set at all.
            for header_value in ["x-real-ip", "cf-connecting-ip", ""] {
                let mut pairs = pairs.clone();
                pairs.push(("RECALL_TRUSTED_IP_HEADER", header_value));
                assert!(
                    matches!(
                        Config::from_lookup(env(&pairs)),
                        Err(ConfigError::TrustedIpHeaderWithTls)
                    ),
                    "{pairs:?}"
                );
            }

            // Unset is the only value that is allowed, and it forces the
            // empty string, not the plain-HTTP default of cf-connecting-ip.
            let cfg = Config::from_lookup(env(&pairs)).unwrap();
            assert_eq!(cfg.trusted_ip_header, "");
        }
    }
}
