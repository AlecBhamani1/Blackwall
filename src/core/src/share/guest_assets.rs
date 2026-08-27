//! Self-contained browser guest assets served by the hosted relay.

/// Guest chat HTML shell.
pub const HTML: &str = include_str!("assets/guest.html");
/// Guest chat stylesheet.
pub const CSS: &str = include_str!("assets/guest.css");
/// Guest chat application script.
pub const JAVASCRIPT: &str = include_str!("assets/guest.js");
