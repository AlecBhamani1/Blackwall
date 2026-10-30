//! TCP ports listening on this computer's loopback or wildcard addresses, read from the operating
//! system without sending anything to them. Addresses bound only to another interface are
//! dropped, so discovery never reaches past this computer.
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
#[cfg(any(target_os = "macos", windows))]
use std::time::Duration;

/// A listening socket: its bound address and port.
pub(super) type Listener = (IpAddr, u16);

/// Whether a bind address is reachable from this computer's loopback.
fn local(address: IpAddr) -> bool {
    let address = match address {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(address, IpAddr::V4),
        v4 => v4,
    };
    address.is_loopback() || address.is_unspecified()
}

/// Rows of Linux `/proc/net/tcp` or `/proc/net/tcp6` in the LISTEN state (`0A`).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(super) fn parse_proc_net_tcp(text: &str) -> Vec<Listener> {
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            if fields.get(3) != Some(&"0A") {
                return None;
            }
            let (address, port) = fields.get(1)?.split_once(':')?;
            let port = u16::from_str_radix(port, 16).ok()?;
            // The kernel prints each 32-bit word of the address in host (little-endian) order.
            let words = (0..address.len() / 8)
                .map(|at| u32::from_str_radix(address.get(at * 8..at * 8 + 8)?, 16).ok())
                .collect::<Option<Vec<_>>>()?;
            let address = match words.as_slice() {
                [word] => IpAddr::V4(Ipv4Addr::from(word.to_le_bytes())),
                [a, b, c, d] => {
                    let [a, b, c, d] = [a, b, c, d].map(|word| word.to_le_bytes());
                    IpAddr::V6(Ipv6Addr::from([
                        a[0], a[1], a[2], a[3], b[0], b[1], b[2], b[3], c[0], c[1], c[2], c[3],
                        d[0], d[1], d[2], d[3],
                    ]))
                }
                _ => return None,
            };
            Some((address, port))
        })
        .filter(|(address, _)| local(*address))
        .collect()
}

/// `host:port` as printed by lsof or netstat: `127.0.0.1:80`, `[::1]:80`, `*:80`.
fn host_port(text: &str) -> Option<Listener> {
    let (host, port) = text.rsplit_once(':')?;
    let port = port.parse().ok()?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let address = if host == "*" {
        IpAddr::V4(Ipv4Addr::UNSPECIFIED)
    } else {
        // Zone suffixes (`fe80::1%lo0`) never name a loopback address we would use.
        host.parse().ok()?
    };
    Some((address, port))
}

/// Output of `lsof -nP -iTCP -sTCP:LISTEN`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(super) fn parse_lsof(text: &str) -> Vec<Listener> {
    text.lines()
        .filter(|line| line.trim_end().ends_with("(LISTEN)"))
        .filter_map(|line| {
            let mut fields = line.split_whitespace().rev();
            fields.next()?;
            host_port(fields.next()?)
        })
        .filter(|(address, _)| local(*address))
        .collect()
}

/// Output of Windows `netstat -ano -p TCP`. Listening rows have a `0.0.0.0:0` or `[::]:0`
/// foreign address, which also covers translated state names.
#[cfg_attr(not(windows), allow(dead_code))]
pub(super) fn parse_netstat(text: &str) -> Vec<Listener> {
    text.lines()
        .filter_map(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            if !fields.first()?.eq_ignore_ascii_case("tcp") {
                return None;
            }
            let foreign = fields.get(2)?;
            let listening = fields.get(3) == Some(&"LISTENING")
                || host_port(foreign)
                    .is_some_and(|(address, port)| port == 0 && address.is_unspecified());
            listening.then_some(())?;
            host_port(fields.get(1)?)
        })
        .filter(|(address, _)| local(*address))
        .collect()
}

/// Run `program`, returning its standard output if it finishes within `limit`.
#[cfg(any(target_os = "macos", windows))]
async fn output(program: &str, arguments: &[&str], limit: Duration) -> Option<String> {
    let mut command = tokio::process::Command::new(program);
    command
        .args(arguments)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    // The listing tools start from the allow-list, not this process's whole environment.
    crate::child_env::from_process(
        crate::child_env::Scope::Owner,
        std::path::Path::new(""),
        &[],
    )
    .apply(&mut command);
    #[cfg(windows)]
    command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW: no console flash from the app.
    let result = tokio::time::timeout(limit, command.output())
        .await
        .ok()?
        .ok()?;
    // Bound what is parsed; a busy computer's listener table is far smaller than this.
    let stdout = result
        .stdout
        .get(..4 * 1024 * 1024)
        .unwrap_or(&result.stdout);
    Some(String::from_utf8_lossy(stdout).into_owned())
}

/// Loopback and wildcard listeners on this computer. Empty when the platform's table or tool
/// is unavailable; discovery then uses only its list of common ports.
pub(super) async fn listening() -> Vec<Listener> {
    #[cfg(target_os = "linux")]
    {
        let mut found = Vec::new();
        for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
            if let Ok(text) = tokio::fs::read_to_string(table).await {
                found.extend(parse_proc_net_tcp(&text));
            }
        }
        found
    }
    #[cfg(target_os = "macos")]
    {
        output(
            "/usr/sbin/lsof",
            &["-nP", "-iTCP", "-sTCP:LISTEN"],
            Duration::from_secs(2),
        )
        .await
        .map(|text| parse_lsof(&text))
        .unwrap_or_default()
    }
    #[cfg(windows)]
    {
        let netstat = std::env::var_os("SystemRoot")
            .map(|root| {
                std::path::Path::new(&root)
                    .join("System32")
                    .join("netstat.exe")
            })
            .filter(|path| path.is_file());
        let program = netstat
            .as_deref()
            .and_then(|path| path.to_str())
            .unwrap_or("netstat.exe");
        output(program, &["-ano", "-p", "TCP"], Duration::from_secs(2))
            .await
            .map(|text| parse_netstat(&text))
            .unwrap_or_default()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        Vec::new()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn ports(found: &[Listener]) -> Vec<u16> {
        found.iter().map(|(_, port)| *port).collect()
    }
    #[test]
    fn proc_net_tcp_keeps_local_listeners_only() {
        let tcp = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:2CAA 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 1 1
   1: 00000000:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 2 1
   2: 0501A8C0:2328 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 3 1
   3: 0100007F:4E20 0100007F:9C40 01 00000000:00000000 00:00000000 00000000  1000        0 4 1
   4: garbage";
        assert_eq!(ports(&parse_proc_net_tcp(tcp)), [11434, 8080]);
        let tcp6 =
            "  sl  local_address                         remote_address                        st
   0: 00000000000000000000000001000000:46A8 00000000000000000000000000000000:0000 0A
   1: 00000000000000000000000000000000:1F40 00000000000000000000000000000000:0000 0A
   2: 0000000000000000FFFF00000100007F:04D2 00000000000000000000000000000000:0000 0A
   3: 000080FE00000000FF005452A1B2C3D4:1388 00000000000000000000000000000000:0000 0A";
        let found = parse_proc_net_tcp(tcp6);
        assert_eq!(ports(&found), [18088, 8000, 1234]);
        assert_eq!(found[0].0, "::1".parse::<IpAddr>().unwrap());
    }
    #[test]
    fn lsof_keeps_local_listeners_only() {
        let text = "COMMAND   PID   USER   FD   TYPE             DEVICE SIZE/OFF NODE NAME
ollama    101 jerkins    3u  IPv4 0x1234      0t0  TCP 127.0.0.1:11434 (LISTEN)
ssh       202 jerkins    5u  IPv6 0x1235      0t0  TCP [::1]:18040 (LISTEN)
ssh       202 jerkins    6u  IPv4 0x1236      0t0  TCP 127.0.0.1:18040 (LISTEN)
Control   303 jerkins    9u  IPv4 0x1237      0t0  TCP *:5000 (LISTEN)
server    404 jerkins    4u  IPv4 0x1238      0t0  TCP 192.168.1.50:9000 (LISTEN)
server    404 jerkins    7u  IPv4 0x1239      0t0  TCP 127.0.0.1:52000->127.0.0.1:11434 (ESTABLISHED)
";
        assert_eq!(ports(&parse_lsof(text)), [11434, 18040, 18040, 5000]);
    }
    #[test]
    fn netstat_keeps_local_listeners_only() {
        let text = "
Active Connections

  Proto  Local Address          Foreign Address        State           PID
  TCP    0.0.0.0:135            0.0.0.0:0              LISTENING       1000
  TCP    127.0.0.1:11434        0.0.0.0:0              LISTENING       2000
  TCP    127.0.0.1:1234         0.0.0.0:0              ABH\u{d6}REN         2001
  TCP    192.168.1.5:9000       0.0.0.0:0              LISTENING       3000
  TCP    127.0.0.1:50000        127.0.0.1:11434        ESTABLISHED     4000
  TCP    [::1]:8080             [::]:0                 LISTENING       5000
";
        assert_eq!(ports(&parse_netstat(text)), [135, 11434, 1234, 8080]);
    }
}
