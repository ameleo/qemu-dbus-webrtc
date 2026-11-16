// SPDX-License-Identifier: MPL-2.0

//! TURN servers the clients may relay their media through (RFC 8656), when they cannot reach
//! the host directly: a network that lets no UDP out, or only TCP or TLS to port 443. The
//! server runs no relay itself; it gives the clients the URIs of one it is told of, coturn or
//! a TURN service, and the credentials of the session.
//!
//! The credentials are fixed ones, or computed for each session from a secret shared with the
//! relay, as in the "TURN REST API" (draft-uberti-behave-turn-rest-00, 2.2), which coturn
//! implements (`use-auth-secret`, `static-auth-secret`): the user name is the time the
//! credentials expire at, in Unix seconds, a colon and a name; the password the base64 of
//! HMAC-SHA1 of the user name, keyed with the secret. The relay checks the password, and that
//! the time has not passed (4.2): the secret never leaves the host, and credentials a client
//! keeps are useless after a day.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use base64::Engine;

/// How long computed credentials last: the draft's one day (2.2). The relay checks them again
/// on each refresh of an allocation, so they must outlast a session; a client that connects
/// again gets new ones.
const LIFETIME: Duration = Duration::from_secs(86_400);
/// The name after the time in a computed user name: the draft's user id, which the relay only
/// logs. It names no account, which the relay, perhaps a third party's, has no need to know.
const USER: &str = "qemu-dbus-webrtc";

/// A TURN server: its URIs, and how its clients authenticate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Server {
    pub urls: Vec<String>,
    pub credentials: Credentials,
}

#[derive(Clone, PartialEq, Eq)]
pub enum Credentials {
    /// Computed for each session from this secret.
    Secret(String),
    /// The same for every session.
    Fixed {
        username: String,
        credential: String,
    },
}

/// Never the secret nor the credential in a log.
impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Secret(_) => f.write_str("Secret(..)"),
            Self::Fixed { username, .. } => f
                .debug_struct("Fixed")
                .field("username", username)
                .finish_non_exhaustive(),
        }
    }
}

impl Server {
    /// The server for a session starting at `now`, as the client is told of it.
    pub fn for_session(&self, now: SystemTime) -> qdw_protocol::v1::TurnServer {
        let (username, credential) = match &self.credentials {
            Credentials::Secret(secret) => computed(secret, now),
            Credentials::Fixed {
                username,
                credential,
            } => (username.clone(), credential.clone()),
        };
        qdw_protocol::v1::TurnServer {
            urls: self.urls.clone(),
            username,
            credential,
        }
    }
}

/// The user name and the password for a session starting at `now`, from `secret`.
fn computed(secret: &str, now: SystemTime) -> (String, String) {
    let expiry = (now + LIFETIME)
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let username = format!("{expiry}:{USER}");
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, secret.as_bytes());
    let tag = ring::hmac::sign(&key, username.as_bytes());
    let password = base64::engine::general_purpose::STANDARD.encode(tag.as_ref());
    (username, password)
}

/// Check `url` is a TURN URI (RFC 7065 3.1):
///
/// ```text
/// turnURI       = scheme ":" host [ ":" port ] [ "?transport=" transport ]
/// scheme        = "turn" / "turns"
/// transport     = "udp" / "tcp" / transport-ext
/// transport-ext = 1*unreserved
/// ```
///
/// The host is a name, an IPv4 address, or an IPv6 one in brackets (RFC 3986 3.2.2).
pub fn check_url(url: &str) -> Result<()> {
    let Some((scheme, rest)) = url.split_once(':') else {
        bail!("{url:?}: not a TURN URI, \"turn:host[:port]\" or \"turns:host[:port]\"");
    };
    if scheme != "turn" && scheme != "turns" {
        bail!("{url:?}: the scheme is \"turn\" or \"turns\" (over TLS)");
    }
    let (authority, query) = match rest.split_once('?') {
        Some((authority, query)) => (authority, Some(query)),
        None => (rest, None),
    };
    let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
        let Some((ip, after)) = bracketed.split_once(']') else {
            bail!("{url:?}: an IPv6 address without its closing bracket");
        };
        if ip.parse::<std::net::Ipv6Addr>().is_err() {
            bail!("{url:?}: {ip:?} is not an IPv6 address");
        }
        match after {
            "" => (ip, None),
            port => match port.strip_prefix(':') {
                Some(port) => (ip, Some(port)),
                None => bail!("{url:?}: unexpected {after:?} after the address"),
            },
        }
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        }
    };
    let unreserved = |c: char| c.is_ascii_alphanumeric() || "-._~".contains(c);
    if host.is_empty() || (!authority.starts_with('[') && !host.chars().all(unreserved)) {
        bail!("{url:?}: no host, or one that is not a name or an address");
    }
    if let Some(port) = port
        && port.parse::<u16>().map_or(true, |p| p == 0)
    {
        bail!("{url:?}: the port is a number from 1 to 65535");
    }
    if let Some(query) = query {
        match query.strip_prefix("transport=") {
            Some(transport) if !transport.is_empty() && transport.chars().all(unreserved) => {}
            _ => bail!("{url:?}: the only parameter is \"?transport=udp\" or \"?transport=tcp\""),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The password is the base64 of HMAC-SHA1 of the user name, keyed with the secret: a value
    /// computed independently, with `printf %s 1700086400:qemu-dbus-webrtc | openssl dgst
    /// -sha1 -hmac north -binary | base64`.
    #[test]
    fn computed_credentials() {
        let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let (username, password) = computed("north", now);
        assert_eq!(username, "1700086400:qemu-dbus-webrtc");
        assert_eq!(password, "LwXb3J4H6DG+yCxi798cnuH0DAM=");
    }

    #[test]
    fn fixed_credentials() {
        let server = Server {
            urls: vec!["turns:relay.example.com:443?transport=tcp".into()],
            credentials: Credentials::Fixed {
                username: "u".into(),
                credential: "p".into(),
            },
        };
        let told = server.for_session(SystemTime::now());
        assert_eq!(told.urls, server.urls);
        assert_eq!(
            (told.username.as_str(), told.credential.as_str()),
            ("u", "p")
        );
    }

    /// Neither the secret nor the credential shows in a log.
    #[test]
    fn credentials_not_logged() {
        let secret = format!("{:?}", Credentials::Secret("hush".into()));
        let fixed = format!(
            "{:?}",
            Credentials::Fixed {
                username: "u".into(),
                credential: "hush".into()
            }
        );
        assert!(
            !secret.contains("hush") && !fixed.contains("hush"),
            "{secret} {fixed}"
        );
    }

    /// The URIs of RFC 7065 3.1, and what it does not allow.
    #[test]
    fn urls() {
        for url in [
            "turn:relay.example.com",
            "turn:relay.example.com:3478",
            "turn:192.0.2.1:3478?transport=udp",
            "turn:[2001:db8::1]:3478?transport=tcp",
            "turn:[2001:db8::1]",
            "turns:relay.example.com:443?transport=tcp",
        ] {
            assert!(check_url(url).is_ok(), "{url}");
        }
        for url in [
            "stun:relay.example.com",
            "relay.example.com:3478",
            "turn:",
            "turn::3478",
            "turn:relay.example.com:0",
            "turn:relay.example.com:70000",
            "turn:relay.example.com:3478?foo=bar",
            "turn:relay.example.com:3478?transport=",
            "turn:[2001:db8::1",
            "turn:[relay]:3478",
            "turn:user@relay.example.com",
        ] {
            assert!(check_url(url).is_err(), "{url}");
        }
    }
}
