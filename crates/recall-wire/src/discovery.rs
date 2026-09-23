//! What a server says about itself, and what a client says about itself.
//!
//! `GET /.well-known/recall` answers with a [`Discovery`] document: which
//! protocol versions the server speaks, which release it is, the oldest
//! client it accepts, how clients may authenticate, and what it can do. It
//! is unauthenticated, like `/health`, and at the path RFC 8615 sets aside
//! for exactly this kind of document.
//!
//! The rules that keep it readable by clients that do not exist yet, taken
//! from git protocol v2, Matrix's `/versions` and MCP:
//!
//! 1. **Two layers.** [`Protocol`] changes only for a breaking change; the
//!    capabilities only ever grow.
//! 2. **Unknown keys are ignored**, at any depth. Nothing here denies
//!    unknown fields, and a client must not either.
//! 3. **Absent means unsupported.** A capability that is not listed is not
//!    available.
//! 4. **A map, not a list.** Each capability is an object, so it can carry
//!    parameters later without a new key.
//! 5. **Nothing is removed** within a protocol version.
//!
//! In the other direction, every request a client sends carries
//! [`PROTOCOL_HEADER`] and a `User-Agent` of the form [`user_agent`] builds.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Where the discovery document is served.
pub const DISCOVERY_PATH: &str = "/.well-known/recall";

/// The protocol version this build speaks.
pub const PROTOCOL: u32 = 1;

/// The request header a client names its protocol version in. A request
/// without it is treated as protocol 1, which is what every client before
/// the header existed spoke.
pub const PROTOCOL_HEADER: &str = "recall-protocol";

/// The discovery document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Discovery {
    /// The protocol versions the server speaks.
    pub protocol: Protocol,
    /// Which build of the server this is.
    pub server: ServerInfo,
    /// The oldest client version the server accepts, as SemVer.
    pub min_client: String,
    /// How clients may authenticate.
    pub auth: Auth,
    /// What the server can do, by name. An absent name is unsupported.
    #[serde(default)]
    pub capabilities: BTreeMap<String, serde_json::Value>,
}

impl Discovery {
    /// Whether the server speaks protocol `version`.
    pub fn speaks(&self, version: u32) -> bool {
        self.protocol.supported.contains(&version)
    }

    /// Whether the server lists the capability `name`.
    pub fn can(&self, name: &str) -> bool {
        self.capabilities.contains_key(name)
    }
}

/// The protocol versions a server speaks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Protocol {
    /// The newest one, which a server's own client speaks.
    pub current: u32,
    /// Every version the server accepts requests in.
    pub supported: Vec<u32>,
}

/// Which build of the server answered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerInfo {
    /// The server's identity, as SemVer: the release version for a release
    /// build, and a `-dev` pre-release of the next patch for anything else.
    pub version: String,
    /// Where the build came from.
    pub build: Build,
}

/// Where a build came from. Provenance only: nothing is decided on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Build {
    /// [`CHANNEL_RELEASE`] for a release build, [`CHANNEL_DEV`] otherwise.
    pub channel: String,
    /// The commit it was built from, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    /// When it was built, in RFC 3339, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<String>,
}

/// How a client may authenticate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Auth {
    /// The methods the server accepts, by name, e.g. [`AUTH_BEARER`].
    pub methods: Vec<String>,
}

/// The one shared bearer token, `RECALL_TOKEN`.
pub const AUTH_BEARER: &str = "bearer";

/// A build made by the release workflow from a release tag.
pub const CHANNEL_RELEASE: &str = "release";

/// Any other build: from `main`, from a pull request, or on someone's
/// machine.
pub const CHANNEL_DEV: &str = "dev";

/// This build's channel, decided at compile time by `build.rs`:
/// `RECALL_BUILD_CHANNEL` when set, otherwise `dev` for a git checkout and
/// `release` for a crate built from crates.io, which is what
/// `cargo install recall` compiles.
pub fn channel() -> &'static str {
    match env!("RECALL_RESOLVED_CHANNEL") {
        CHANNEL_RELEASE => CHANNEL_RELEASE,
        _ => CHANNEL_DEV,
    }
}

/// The commit this build was compiled from, when the build recorded one.
pub fn revision() -> Option<&'static str> {
    option_env!("RECALL_GIT_COMMIT").filter(|r| !r.is_empty())
}

/// When this build was made, when the build recorded it.
pub fn created() -> Option<&'static str> {
    option_env!("RECALL_BUILD_CREATED").filter(|c| !c.is_empty())
}

/// This build's version, as SemVer: see [`version_for`].
pub fn version() -> String {
    version_for(channel(), revision())
}

/// The version a build of this source reports.
///
/// A release build reports its release. Anything else reports a
/// pre-release of the next patch, with the commit as build metadata, so
/// `0.3.2` built from a later `main` reads `0.3.3-dev+g1a2b3c4`. SemVer
/// orders that after `0.3.2` and before `0.3.3`, which is where the code
/// actually sits, and ignores the metadata when comparing.
pub fn version_for(channel: &str, revision: Option<&str>) -> String {
    let base = env!("CARGO_PKG_VERSION");
    if channel == CHANNEL_RELEASE {
        return base.to_string();
    }
    let next = match Version::parse(base) {
        Some(v) => format!("{}.{}.{}", v.major, v.minor, v.patch + 1),
        None => base.to_string(),
    };
    match revision {
        Some(rev) => {
            let short: String = rev.chars().take(7).collect();
            format!("{next}-dev+g{short}")
        }
        None => format!("{next}-dev"),
    }
}

/// The `User-Agent` a client sends: `recall/<version> (<os>-<arch>)`, the
/// way git sends `agent=git/<version>`.
pub fn user_agent() -> String {
    format!(
        "recall/{} ({}-{})",
        version(),
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}

/// A SemVer version, enough of it to compare two.
///
/// Build metadata is dropped on parsing: SemVer says it *"MUST be ignored
/// when determining version precedence"*.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    /// Major.
    pub major: u64,
    /// Minor.
    pub minor: u64,
    /// Patch.
    pub patch: u64,
    /// Pre-release identifiers, empty for a release.
    pub pre: Vec<String>,
}

impl Version {
    /// Parses `1.2.3`, `1.2.3-dev`, `1.2.3-rc.1+build`. [`None`] for
    /// anything else.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim().trim_start_matches('v');
        let text = text.split('+').next()?;
        let (core, pre) = match text.split_once('-') {
            Some((core, pre)) => (core, pre.split('.').map(str::to_string).collect()),
            None => (text, Vec::new()),
        };
        let mut parts = core.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        Some(Self {
            major,
            minor,
            patch,
            pre,
        })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    /// SemVer §11: major, minor and patch numerically; then a version with
    /// a pre-release before the same version without one; then pre-release
    /// identifiers left to right, numeric ones numerically and before
    /// alphanumeric ones, and a shorter list before a longer one it prefixes.
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        let core =
            (self.major, self.minor, self.patch).cmp(&(other.major, other.minor, other.patch));
        if core != Ordering::Equal {
            return core;
        }
        match (self.pre.is_empty(), other.pre.is_empty()) {
            (true, true) => return Ordering::Equal,
            (true, false) => return Ordering::Greater,
            (false, true) => return Ordering::Less,
            (false, false) => {}
        }
        for (a, b) in self.pre.iter().zip(&other.pre) {
            let order = match (a.parse::<u64>(), b.parse::<u64>()) {
                (Ok(x), Ok(y)) => x.cmp(&y),
                (Ok(_), Err(_)) => Ordering::Less,
                (Err(_), Ok(_)) => Ordering::Greater,
                (Err(_), Err(_)) => a.cmp(b),
            };
            if order != Ordering::Equal {
                return order;
            }
        }
        self.pre.len().cmp(&other.pre.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    /// The ordering examples from SemVer §11, in order.
    #[test]
    fn versions_order_the_way_semver_says() {
        let ordered = [
            "1.0.0-alpha",
            "1.0.0-alpha.1",
            "1.0.0-alpha.beta",
            "1.0.0-beta",
            "1.0.0-beta.2",
            "1.0.0-beta.11",
            "1.0.0-rc.1",
            "1.0.0",
            "2.0.0",
            "2.1.0",
            "2.1.1",
        ];
        for pair in ordered.windows(2) {
            assert!(v(pair[0]) < v(pair[1]), "{} < {}", pair[0], pair[1]);
        }
    }

    #[test]
    fn build_metadata_does_not_count() {
        assert_eq!(v("0.3.3-dev+g1a2b3c4"), v("0.3.3-dev+gffffff0"));
        assert!(v("0.3.2") < v("0.3.3-dev+g1a2b3c4"));
        assert!(v("0.3.3-dev+g1a2b3c4") < v("0.3.3"));
    }

    #[test]
    fn what_is_not_a_version_is_refused() {
        for bad in ["", "1", "1.2", "1.2.3.4", "a.b.c", "1.2.x"] {
            assert_eq!(Version::parse(bad), None, "{bad:?}");
        }
        assert_eq!(v("v1.2.3"), v("1.2.3"));
    }

    #[test]
    fn a_release_build_is_its_release_and_anything_else_the_next_dev() {
        let base = env!("CARGO_PKG_VERSION");
        assert_eq!(version_for(CHANNEL_RELEASE, Some("abc")), base);
        let dev = version_for(CHANNEL_DEV, Some("e100cfdd88e8a0e6659b"));
        assert!(dev.ends_with("-dev+ge100cfd"), "{dev}");
        assert!(v(base) < v(&dev), "{base} < {dev}");
        assert!(version_for(CHANNEL_DEV, None).ends_with("-dev"));
    }

    /// A document from a newer server, with a key and a capability this
    /// build has never heard of, still reads, and still answers.
    #[test]
    fn unknown_keys_are_ignored_and_absent_capabilities_are_unsupported() {
        let doc: Discovery = serde_json::from_str(
            r#"{
                "protocol": {"current": 2, "supported": [1, 2]},
                "server": {"version": "0.9.0", "build": {"channel": "release", "signed": true}},
                "min_client": "0.3.0",
                "auth": {"methods": ["device-sig-v1", "bearer"]},
                "capabilities": {"merge_base": {}, "telepathy": {"level": 3}},
                "operator": {"contact": "someone"}
            }"#,
        )
        .unwrap();
        assert!(doc.speaks(1) && doc.speaks(2) && !doc.speaks(3));
        assert!(doc.can("merge_base") && doc.can("telepathy"));
        assert!(!doc.can("scopes"));
    }

    #[test]
    fn the_user_agent_names_the_version_and_platform() {
        let ua = user_agent();
        assert!(ua.starts_with("recall/"), "{ua}");
        assert!(ua.contains(std::env::consts::OS), "{ua}");
    }
}
