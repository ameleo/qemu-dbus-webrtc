// SPDX-License-Identifier: MPL-2.0

//! The console page: the server serves it to its viewers, and a connection broker built on
//! qemu-dbus-webrtc serves this same page, from the version it depends on, rather than one a host
//! sends it.
//!
//! The page is static files, served next to each other under the VM's path (`/vm/<uuid>/`),
//! with no inline script or style: its Content Security Policy, `POLICY`, is the same whatever
//! the version.

/// A file of the page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct File {
    /// Relative to the VM's page, without a leading "/": "index.html".
    pub path: &'static str,
    /// Its media type, with its charset.
    pub content_type: &'static str,
    pub content: &'static str,
}

/// The page's files, its HTML first.
pub const FILES: [File; 3] = [
    File {
        path: "index.html",
        content_type: "text/html; charset=utf-8",
        content: include_str!("../index.html"),
    },
    File {
        path: "app.js",
        content_type: "text/javascript; charset=utf-8",
        content: include_str!("../app.js"),
    },
    File {
        path: "app.css",
        content_type: "text/css; charset=utf-8",
        content: include_str!("../app.css"),
    },
];

/// The page's Content Security Policy: its own script and style only, the guest pointer's
/// images as data URLs, connections to its own origin only (the signalling WebSocket), and no
/// framing.
pub const POLICY: &str = "default-src 'none'; script-src 'self'; style-src 'self'; \
     img-src data:; connect-src 'self'; base-uri 'none'; form-action 'none'; \
     frame-ancestors 'none'";

/// A file of the page by its path: "" is its HTML.
pub fn file(path: &str) -> Option<&'static File> {
    let path = if path.is_empty() { "index.html" } else { path };
    FILES.iter().find(|file| file.path == path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Inline script or style would need the policy to name them: none is allowed, nor event
    /// handler or style attributes.
    #[test]
    fn no_inline_script_or_style() {
        let html = file("").unwrap().content;
        assert_eq!(html.matches("<script").count(), 1);
        assert!(html.contains("<script src=\"app.js\"></script>"));
        assert!(!html.contains("<style"));
        assert!(!html.contains(" style="));
        let attribute = event_handler(html);
        assert!(attribute.is_none(), "{attribute:?}");
    }

    /// The first ` on…=` attribute of the HTML, if any.
    fn event_handler(html: &str) -> Option<&str> {
        html.match_indices(" on")
            .map(|(i, _)| &html[i + 1..])
            .find_map(|rest| {
                let name: String = rest.chars().take_while(char::is_ascii_lowercase).collect();
                (name.len() > 2 && rest[name.len()..].starts_with('='))
                    .then_some(&rest[..name.len()])
            })
    }

    #[test]
    fn every_file_the_html_names() {
        let html = file("index.html").unwrap().content;
        for name in ["app.js", "app.css"] {
            assert!(html.contains(&format!("\"{name}\"")), "{name}");
            assert!(file(name).is_some(), "{name}");
        }
        assert!(file("../index.html").is_none());
    }

    #[test]
    fn the_policy_allows_nothing_inline() {
        assert!(!POLICY.contains("unsafe"));
        assert!(!POLICY.contains("sha256-"));
        assert!(POLICY.contains("frame-ancestors 'none'"));
    }
}
