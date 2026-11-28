//! Web pages: `browser-host` runs Servo and draws pages for the browser app.
//!
//! browser-host claims `web`. An app opens a page with `open`, a streamed request
//! that lasts as long as the page: its first part says the page's id, later parts
//! say where it went, its title, how loading is going and whether it can go back.
//! Cancelling it closes the page. The app shows the page in a `surface` node with
//! `service: "web"` and that id, and the compositor attaches to it ([`crate::surface`]).
//!
//! A page belongs to the app that opened it; only that app (or a system service
//! acting for it with `"as"`) may steer it, attach to it or send it input. Apps
//! reach `web` only with the `service:web` grant. browser-host fetches pages
//! itself, so the app needs no network grant.

use serde::{Deserialize, Serialize};

/// The name browser-host claims.
pub const SERVICE: &str = "web";

/// Most pages browser-host keeps open at once (each can take hundreds of MB).
pub const MAX_PAGES: usize = 8;
/// The longest address it takes.
pub const MAX_URL: usize = 8192;

/// browser-host's methods, beside the surface methods ([`crate::surface::method`]).
///
/// | method     | params             | result |
/// |------------|--------------------|--------|
/// | `open`     | [`OpenParams`]     | streamed [`PageEvent`]s |
/// | `navigate` | [`NavigateParams`] | null |
/// | `back`     | [`PageParams`]     | null |
/// | `forward`  | [`PageParams`]     | null |
/// | `reload`   | [`PageParams`]     | null |
pub mod method {
    pub const OPEN: &str = "open";
    pub const NAVIGATE: &str = "navigate";
    pub const BACK: &str = "back";
    pub const FORWARD: &str = "forward";
    pub const RELOAD: &str = "reload";
}

/// Params of `open`: where to start, if anywhere (a blank page otherwise).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct OpenParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// Params of `navigate`: what was typed in the address bar. An address without a
/// scheme gets `https://`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct NavigateParams {
    pub id: String,
    pub url: String,
}

/// Params of `back`, `forward` and `reload`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PageParams {
    pub id: String,
}

/// How far a page has loaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum Load {
    Started,
    /// Its `<head>` is read: the title and the start of the page are coming.
    Parsed,
    /// It and everything on it have loaded.
    Complete,
}

/// A part of `open`'s streamed answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "event", rename_all = "lowercase")]
pub enum PageEvent {
    /// The first part: the page's id, for the `surface` node and the methods.
    Opened {
        id: String,
    },
    /// The page is at `url` now.
    Url {
        url: String,
    },
    /// The page's title (none if it has none).
    Title {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    Load {
        load: Load,
    },
    /// Whether there's somewhere to go back or forward to.
    History {
        back: bool,
        forward: bool,
    },
    /// The page's content process failed; the stream ends after this.
    Crashed {
        reason: String,
    },
}

/// What browser-host loads for `typed`: the address as given if it has a scheme,
/// else with `https://` in front. None for nothing, or something too long.
pub fn fix_address(typed: &str) -> Option<String> {
    let t = typed.trim();
    if t.is_empty() || t.len() > MAX_URL {
        return None;
    }
    let has_scheme = t.split_once(':').is_some_and(|(s, rest)| {
        !s.is_empty()
            && s.chars().all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
            && (rest.starts_with("//") || s == "about" || s == "data")
    });
    Some(if has_scheme { t.to_owned() } else { format!("https://{t}") })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_address_without_a_scheme_gets_https() {
        assert_eq!(fix_address(" example.com ").as_deref(), Some("https://example.com"));
        assert_eq!(fix_address("localhost:8080/x").as_deref(), Some("https://localhost:8080/x"));
        assert_eq!(fix_address("http://a.test").as_deref(), Some("http://a.test"));
        assert_eq!(fix_address("about:blank").as_deref(), Some("about:blank"));
        assert_eq!(fix_address("   "), None);
        assert_eq!(fix_address(&"a".repeat(MAX_URL + 1)), None);
    }

    #[test]
    fn page_events_read_as_documented() {
        let e: PageEvent = serde_json::from_value(json!({"event": "load", "load": "complete"})).unwrap();
        assert_eq!(e, PageEvent::Load { load: Load::Complete });
        assert_eq!(
            serde_json::to_value(PageEvent::Title { title: None }).unwrap(),
            json!({"event": "title"})
        );
    }
}
