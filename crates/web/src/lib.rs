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

    /// The HTML's elements: each tag's name and attributes, lowercase names, values unquoted.
    fn tags(html: &str) -> Vec<(String, Vec<(String, String)>)> {
        let mut tags = Vec::new();
        let mut rest = html;
        while let Some(start) = rest.find('<') {
            rest = &rest[start + 1..];
            if let Some(comment) = rest.strip_prefix("!--") {
                rest = comment.split_once("-->").map_or("", |(_, after)| after);
                continue;
            }
            if !rest.starts_with(|c: char| c.is_ascii_alphabetic()) {
                continue;
            }
            let end = rest
                .find(|c: char| c.is_whitespace() || c == '>' || c == '/')
                .unwrap_or(rest.len());
            let name = rest[..end].to_ascii_lowercase();
            rest = &rest[end..];
            let mut attributes = Vec::new();
            loop {
                rest = rest.trim_start_matches(|c: char| c.is_whitespace() || c == '/');
                if rest.is_empty() || rest.starts_with('>') {
                    break;
                }
                let end = rest
                    .find(|c: char| c.is_whitespace() || c == '=' || c == '>' || c == '/')
                    .unwrap_or(rest.len());
                let attribute = rest[..end].to_ascii_lowercase();
                rest = rest[end..].trim_start();
                let mut value = String::new();
                if let Some(after) = rest.strip_prefix('=') {
                    let after = after.trim_start();
                    let (text, left) = match after.chars().next() {
                        Some(quote @ ('"' | '\'')) => {
                            after[1..].split_once(quote).unwrap_or((&after[1..], ""))
                        }
                        _ => after.split_at(
                            after
                                .find(|c: char| c.is_whitespace() || c == '>')
                                .unwrap_or(after.len()),
                        ),
                    };
                    value = text.to_owned();
                    rest = left;
                }
                attributes.push((attribute, value));
            }
            tags.push((name, attributes));
        }
        tags
    }

    /// Inline script or style would need the policy to name them: none is allowed, nor event
    /// handler or style attributes, wherever they are written.
    #[test]
    fn no_inline_script_or_style() {
        let html = file("").unwrap().content;
        let elements = tags(html);
        assert!(elements.iter().all(|(name, _)| name != "style"));
        for (name, attributes) in &elements {
            for (attribute, _) in attributes {
                assert!(
                    attribute != "style" && !attribute.starts_with("on"),
                    "<{name} {attribute}=…>"
                );
            }
        }
        // Each script is a file: its element is empty.
        let mut rest = html;
        while let Some(start) = rest.find("<script") {
            let element = &rest[start..];
            let open = element.find('>').unwrap() + 1;
            assert!(element[open..].starts_with("</script>"), "an inline script");
            assert!(element[..open].contains(" src="), "a script without src");
            rest = &element[open..];
        }
        // The parser sees through any whitespace.
        let sample = tags("<div\n  onclick=\"x()\"\tstyle='a'>");
        assert_eq!(sample[0].1[0].0, "onclick");
        assert_eq!(sample[0].1[1], ("style".into(), "a".into()));
    }

    /// Every file the HTML refers to is one of the page's.
    #[test]
    fn every_file_the_html_names() {
        let html = file("index.html").unwrap().content;
        let mut named = Vec::new();
        for (_, attributes) in tags(html) {
            for (attribute, value) in attributes {
                if attribute == "src" || attribute == "href" {
                    assert!(file(&value).is_some(), "{value}");
                    named.push(value);
                }
            }
        }
        named.sort();
        assert_eq!(named, ["app.css", "app.js"]);
        assert!(file("../index.html").is_none());
    }

    /// Scripts and styles from the page's own origin only: no hash, nonce or 'unsafe-…' source.
    #[test]
    fn the_policy_allows_nothing_inline() {
        let directive = |name: &str| -> Vec<&str> {
            POLICY
                .split(';')
                .map(|d| d.split_whitespace().collect::<Vec<_>>())
                .find(|d| d.first() == Some(&name))
                .unwrap_or_else(|| panic!("{name}"))[1..]
                .to_vec()
        };
        assert_eq!(directive("default-src"), ["'none'"]);
        assert_eq!(directive("script-src"), ["'self'"]);
        assert_eq!(directive("style-src"), ["'self'"]);
        assert_eq!(directive("frame-ancestors"), ["'none'"]);
    }
}
