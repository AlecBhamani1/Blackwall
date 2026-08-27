//! Tailnet-safe listener and advertised-URL selection.

use std::{
    collections::HashSet,
    io::{self, Read},
    net::{IpAddr, Ipv4Addr},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde::Deserialize;

#[cfg(target_os = "macos")]
use std::path::Path;

const STATUS_COMMAND_TIMEOUT: Duration = Duration::from_secs(2);
const STATUS_POLL_INTERVAL: Duration = Duration::from_millis(10);
const MAX_STATUS_BYTES: usize = 1024 * 1024;

/// Information about the interface selected for a guest listener.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ShareNetwork {
    /// Exact address the listener may bind to. This is never unspecified.
    pub bind_ip: Ipv4Addr,
    /// Short, user-facing description of who can reach the listener.
    pub label: &'static str,
}

/// Chooses a verified Tailscale IPv4 when one is present, otherwise loopback.
///
/// A `100.64.0.0/10` address alone is not proof of Tailscale ownership because
/// that range is shared CGNAT space. A candidate must also be reported as this
/// node's address by the read-only `tailscale status` command and be present on
/// a local interface. Status failures deliberately fall back to loopback.
pub fn detect_share_network() -> ShareNetwork {
    let local_ipv4s = local_interface_ipv4s();
    let status = read_tailscale_status();
    select_share_network(&local_ipv4s, status.as_deref())
}

fn local_interface_ipv4s() -> Vec<Ipv4Addr> {
    if_addrs::get_if_addrs()
        .map(|interfaces| {
            interfaces
                .into_iter()
                .filter_map(|interface| match interface.ip() {
                    IpAddr::V4(ip) => Some(ip),
                    IpAddr::V6(_) => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

fn select_share_network(local_ipv4s: &[Ipv4Addr], status_json: Option<&[u8]>) -> ShareNetwork {
    let bind_ip = status_json
        .and_then(parse_running_tailscale_ipv4s)
        .and_then(|reported_ipv4s| correlate_local_ipv4(local_ipv4s, &reported_ipv4s));

    match bind_ip {
        Some(bind_ip) => ShareNetwork {
            bind_ip,
            label: "Tailscale tailnet",
        },
        None => loopback_network(),
    }
}

fn loopback_network() -> ShareNetwork {
    ShareNetwork {
        bind_ip: Ipv4Addr::LOCALHOST,
        label: "This Mac only",
    }
}

#[derive(Debug, Deserialize)]
struct TailscaleStatus {
    #[serde(rename = "BackendState", default)]
    backend_state: String,
    #[serde(rename = "TUN", default)]
    tun: bool,
    #[serde(rename = "TailscaleIPs", default)]
    tailscale_ips: Vec<IpAddr>,
    #[serde(rename = "Self", default)]
    self_node: Option<TailscaleSelf>,
}

#[derive(Debug, Deserialize)]
struct TailscaleSelf {
    #[serde(rename = "TailscaleIPs", default)]
    tailscale_ips: Vec<IpAddr>,
}

fn parse_running_tailscale_ipv4s(status_json: &[u8]) -> Option<Vec<Ipv4Addr>> {
    let status: TailscaleStatus = serde_json::from_slice(status_json).ok()?;
    if status.backend_state != "Running" || !status.tun {
        return None;
    }

    let mut reported = status.tailscale_ips;
    if let Some(self_node) = status.self_node {
        reported.extend(self_node.tailscale_ips);
    }

    Some(
        reported
            .into_iter()
            .filter_map(|ip| match ip {
                IpAddr::V4(ip) if is_tailscale_ipv4(ip) => Some(ip),
                IpAddr::V4(_) | IpAddr::V6(_) => None,
            })
            .collect(),
    )
}

fn correlate_local_ipv4(local_ipv4s: &[Ipv4Addr], reported_ipv4s: &[Ipv4Addr]) -> Option<Ipv4Addr> {
    let local_ipv4s: HashSet<_> = local_ipv4s.iter().copied().collect();
    reported_ipv4s
        .iter()
        .copied()
        .find(|ip| local_ipv4s.contains(ip))
}

/// Queries the existing Tailscale daemon through its CLI/LocalAPI bridge.
///
/// `status` is read-only: this never runs `up`, `serve`, `funnel`, or another
/// command that starts or reconfigures Tailscale. Runtime and output are bounded
/// so an unavailable daemon cannot indefinitely block guest-share startup.
fn read_tailscale_status() -> Option<Vec<u8>> {
    let mut child = tailscale_status_command()
        .args(["status", "--json", "--peers=false"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;
    let reader = match thread::Builder::new()
        .name("blackwall-tailscale-status".to_owned())
        .spawn(move || read_status_output(stdout))
    {
        Ok(reader) => reader,
        Err(_) => {
            terminate_child(&mut child);
            return None;
        }
    };

    let started = Instant::now();
    let exit_status = loop {
        match child.try_wait() {
            Ok(Some(exit_status)) => break Some(exit_status),
            Ok(None) if started.elapsed() < STATUS_COMMAND_TIMEOUT => {
                thread::sleep(STATUS_POLL_INTERVAL);
            }
            Ok(None) | Err(_) => {
                terminate_child(&mut child);
                break None;
            }
        }
    };
    let output = reader.join().ok()?.ok()?;

    exit_status.filter(std::process::ExitStatus::success)?;
    Some(output)
}

fn tailscale_status_command() -> Command {
    #[cfg(target_os = "macos")]
    {
        // GUI apps launched by Finder do not normally inherit Homebrew or
        // /usr/local/bin in PATH. The macOS client embeds the same read-only
        // CLI used by its optional command-line wrapper.
        const MACOS_CANDIDATES: [&str; 3] = [
            "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
            "/usr/local/bin/tailscale",
            "/opt/homebrew/bin/tailscale",
        ];
        if let Some(path) = MACOS_CANDIDATES
            .into_iter()
            .map(Path::new)
            .find(|path| path.is_file())
        {
            return Command::new(path);
        }
    }

    Command::new("tailscale")
}

fn read_status_output(stdout: std::process::ChildStdout) -> io::Result<Vec<u8>> {
    let mut output = Vec::with_capacity(16 * 1024);
    stdout
        .take((MAX_STATUS_BYTES + 1) as u64)
        .read_to_end(&mut output)?;
    if output.len() > MAX_STATUS_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Tailscale status output exceeded the safety limit",
        ));
    }
    Ok(output)
}

fn terminate_child(child: &mut std::process::Child) {
    let _kill_result = child.kill();
    let _wait_result = child.wait();
}

/// Returns whether an address is inside Tailscale's CGNAT range.
pub fn is_tailscale_ipv4(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    octets[0] == 100 && (64..=127).contains(&octets[1])
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUNNING_STATUS: &[u8] = br#"{
        "BackendState": "Running",
        "TUN": true,
        "TailscaleIPs": ["100.91.2.3", "fd7a:115c:a1e0::1"],
        "Self": { "TailscaleIPs": ["100.91.2.3"] }
    }"#;

    #[test]
    fn recognizes_only_the_tailscale_cgnat_range() {
        assert!(is_tailscale_ipv4(Ipv4Addr::new(100, 64, 0, 1)));
        assert!(is_tailscale_ipv4(Ipv4Addr::new(100, 100, 10, 20)));
        assert!(is_tailscale_ipv4(Ipv4Addr::new(100, 127, 255, 254)));
        assert!(!is_tailscale_ipv4(Ipv4Addr::new(100, 63, 255, 255)));
        assert!(!is_tailscale_ipv4(Ipv4Addr::new(100, 128, 0, 0)));
        assert!(!is_tailscale_ipv4(Ipv4Addr::LOCALHOST));
        assert!(!is_tailscale_ipv4(Ipv4Addr::UNSPECIFIED));
    }

    #[test]
    fn selects_only_an_address_confirmed_by_status_and_a_local_interface() {
        let arbitrary_cgnat = Ipv4Addr::new(100, 70, 1, 1);
        let verified_tailscale = Ipv4Addr::new(100, 91, 2, 3);

        assert_eq!(
            select_share_network(
                &[arbitrary_cgnat, verified_tailscale, Ipv4Addr::LOCALHOST],
                Some(RUNNING_STATUS),
            ),
            ShareNetwork {
                bind_ip: verified_tailscale,
                label: "Tailscale tailnet",
            }
        );
    }

    #[test]
    fn arbitrary_cgnat_without_status_correlation_falls_back_to_loopback() {
        let arbitrary_cgnat = Ipv4Addr::new(100, 70, 1, 1);

        assert_eq!(
            select_share_network(&[arbitrary_cgnat], Some(RUNNING_STATUS)),
            loopback_network()
        );
        assert_eq!(
            select_share_network(&[arbitrary_cgnat], None),
            loopback_network()
        );
    }

    #[test]
    fn stopped_userspace_or_malformed_status_falls_back_to_loopback() {
        let local = [Ipv4Addr::new(100, 91, 2, 3)];
        let stopped = br#"{
            "BackendState": "Stopped",
            "TUN": true,
            "TailscaleIPs": ["100.91.2.3"]
        }"#;
        let userspace = br#"{
            "BackendState": "Running",
            "TUN": false,
            "TailscaleIPs": ["100.91.2.3"]
        }"#;

        assert_eq!(
            select_share_network(&local, Some(stopped)),
            loopback_network()
        );
        assert_eq!(
            select_share_network(&local, Some(userspace)),
            loopback_network()
        );
        assert_eq!(
            select_share_network(&local, Some(b"not json")),
            loopback_network()
        );
    }

    #[test]
    fn self_status_addresses_are_supported_when_top_level_addresses_are_absent() {
        let local = Ipv4Addr::new(100, 88, 4, 9);
        let status = br#"{
            "BackendState": "Running",
            "TUN": true,
            "Self": { "TailscaleIPs": ["100.88.4.9"] }
        }"#;

        assert_eq!(select_share_network(&[local], Some(status)).bind_ip, local);
    }
}
