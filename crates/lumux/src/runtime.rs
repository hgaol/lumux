//! Pane runtime context shared by detached control commands.
//!
//! Panes receive a transport-independent `$LUMUX` endpoint from the daemon.
//! Transport-specific variables remain supported for commands launched outside
//! a pane, but hook adapters should only need the pane contract.

use std::ffi::{OsStr, OsString};

fn endpoint_with(
    transport_key: &str,
    getenv: impl Fn(&str) -> Option<OsString>,
) -> Option<OsString> {
    getenv("LUMUX")
        .filter(|value| !value.is_empty() && value != OsStr::new("1"))
        .or_else(|| getenv(transport_key).filter(|value| !value.is_empty()))
}

#[cfg(unix)]
pub(crate) fn socket_path() -> std::path::PathBuf {
    endpoint_with("LUMUX_SOCK", |key| std::env::var_os(key))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(lumux_backend_unix::default_socket_path)
}

#[cfg(windows)]
pub(crate) fn pipe_path() -> String {
    endpoint_with("LUMUX_PIPE", |key| std::env::var_os(key))
        .and_then(|value| value.into_string().ok())
        .unwrap_or_else(lumux_backend_win::default_pipe_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |key| {
            pairs
                .iter()
                .find(|(candidate, _)| *candidate == key)
                .map(|(_, value)| OsString::from(*value))
        }
    }

    #[test]
    fn pane_endpoint_precedes_transport_specific_fallback() {
        assert_eq!(
            endpoint_with(
                "LUMUX_SOCK",
                env_of(&[
                    ("LUMUX", "/runtime/pane.sock"),
                    ("LUMUX_SOCK", "/legacy.sock")
                ])
            ),
            Some(OsString::from("/runtime/pane.sock"))
        );
    }

    #[test]
    fn sentinel_uses_transport_specific_endpoint_or_default() {
        assert_eq!(
            endpoint_with(
                "LUMUX_SOCK",
                env_of(&[("LUMUX", "1"), ("LUMUX_SOCK", "/legacy.sock")])
            ),
            Some(OsString::from("/legacy.sock"))
        );
        assert_eq!(endpoint_with("LUMUX_SOCK", env_of(&[("LUMUX", "1")])), None);
    }
}
