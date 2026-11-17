//! This Mac's name, sent with lock requests (`X-Drive-Device`) so the Drive can tell a member's
//! two Macs apart: a lock held on one Mac is refused on the other, with that Mac's name.

use std::process::Command;

/// The header lock requests carry.
pub const DEVICE_HEADER: &str = "X-Drive-Device";
/// Longest device name the server accepts, in characters.
pub const MAX_DEVICE_LEN: usize = 64;

/// `raw` as a header-safe device name: runs of whitespace made one space, other control characters
/// dropped, curly quotes made straight, other non-ASCII characters dropped (HTTP header values are
/// ASCII), trimmed and cut to [`MAX_DEVICE_LEN`] characters. Can be empty, and then no header is
/// sent.
pub fn sanitize_device_name(raw: &str) -> String {
    let ascii: String = raw
        .chars()
        .filter_map(|c| match c {
            '\u{2018}' | '\u{2019}' => Some('\''),
            '\u{201C}' | '\u{201D}' => Some('"'),
            c if c.is_whitespace() => Some(' '),
            c if c.is_ascii() && !c.is_ascii_control() => Some(c),
            _ => None,
        })
        .collect();
    ascii.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(MAX_DEVICE_LEN).collect::<String>().trim_end().to_string()
}

/// This computer's name (macOS: the Sharing "Computer Name"; elsewhere the host name), sanitized.
/// Empty when it can't be read.
pub fn local_device_name() -> String {
    let attempts: &[(&str, &[&str])] =
        if cfg!(target_os = "macos") { &[("scutil", &["--get", "ComputerName"]), ("hostname", &[])] } else { &[("hostname", &[])] };
    attempts
        .iter()
        .filter_map(|(cmd, args)| Command::new(cmd).args(*args).output().ok())
        .filter(|out| out.status.success())
        .map(|out| sanitize_device_name(&String::from_utf8_lossy(&out.stdout)))
        .find(|name| !name.is_empty())
        .unwrap_or_default()
}
