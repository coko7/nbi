//! Thin wrapper around the `netbird` CLI plus parsers for its output.

use std::env;
use std::process::{Command, Output, Stdio};

use anyhow::{Context, Result, bail};
use serde_json::Value;

#[derive(Clone, Debug)]
pub struct Netbird {
    /// Program + leading args, e.g. `["netbird"]` or `["sudo", "-n", "netbird"]`.
    argv: Vec<String>,
}

impl Netbird {
    /// Honour `$NB` (like the shell script) and fall back to passwordless sudo
    /// when the daemon socket refuses the current user.
    pub fn detect() -> Self {
        let argv = env::var("NB")
            .ok()
            .map(|s| s.split_whitespace().map(str::to_owned).collect::<Vec<_>>())
            .filter(|argv| !argv.is_empty())
            .unwrap_or_else(|| vec!["netbird".to_owned()]);
        let nb = Self { argv };
        if nb.succeeds(&["status"]) || !passwordless_sudo() {
            return nb;
        }
        let mut argv = vec!["sudo".to_owned(), "-n".to_owned()];
        argv.extend(nb.argv);
        Self { argv }
    }

    /// A netbird stand-in whose every call fails, for UI tests.
    #[cfg(test)]
    pub fn fake() -> Self {
        Self { argv: vec!["false".to_owned()] }
    }

    pub fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(&self.argv[0]);
        cmd.args(&self.argv[1..]).args(args).stdin(Stdio::null());
        cmd
    }

    /// Human-readable command line, for logs.
    pub fn display(&self, args: &[&str]) -> String {
        self.argv
            .iter()
            .map(String::as_str)
            .chain(args.iter().copied())
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn output(&self, args: &[&str]) -> Result<Output> {
        self.command(args)
            .output()
            .with_context(|| format!("failed to run `{}`", self.display(args)))
    }

    fn succeeds(&self, args: &[&str]) -> bool {
        self.command(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    /// Run and return stdout; a non-zero exit becomes an error with the most useful stderr line.
    fn stdout(&self, args: &[&str]) -> Result<String> {
        let out = self.output(args)?;
        if !out.status.success() {
            bail!("{}", error_summary(&out));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    pub fn version(&self) -> String {
        self.stdout(&["version"])
            .map(|v| v.trim().to_owned())
            .unwrap_or_else(|_| "n/a".to_owned())
    }

    pub fn status(&self) -> Status {
        let out = match self.output(&["status", "--json"]) {
            Ok(out) => out,
            Err(err) => return Status::unreachable(format!("{err:#}")),
        };
        if !out.status.success() {
            return Status::unreachable(error_summary(&out));
        }
        let text = String::from_utf8_lossy(&out.stdout);
        match serde_json::from_str::<Value>(&text) {
            Ok(json) => Status::from_json(&json),
            // e.g. "Daemon status: NeedsLogin" is printed as plain text even with --json
            Err(_) => Status::from_text(&text),
        }
    }

    pub fn detail(&self) -> Result<String> {
        self.stdout(&["status", "--detail"])
    }

    pub fn profiles(&self) -> Result<Vec<Profile>> {
        Ok(parse_profiles(&self.stdout(&["profile", "list"])?))
    }

    pub fn routes(&self) -> Result<Vec<Route>> {
        Ok(parse_routes(&self.stdout(&["routes", "list"])?))
    }

    /// Active profile name: status JSON first, then the ✓ in `profile list`, then "default".
    pub fn resolve_profile(&self, status: &Status) -> String {
        status
            .profile
            .clone()
            .or_else(|| active_profile(&self.profiles().ok()?))
            .unwrap_or_else(|| "default".to_owned())
    }
}

fn passwordless_sudo() -> bool {
    Command::new("sudo")
        .args(["-n", "true"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Prefer netbird's own `Error: ...` line, else the last non-empty line of output.
fn error_summary(out: &Output) -> String {
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines = || {
        stderr
            .lines()
            .chain(stdout.lines())
            .map(str::trim)
            .filter(|l| !l.is_empty())
    };
    lines()
        .find(|l| l.starts_with("Error:"))
        .or_else(|| lines().next_back())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("netbird exited with {}", out.status))
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
pub struct Status {
    /// The daemon answered at all.
    pub reachable: bool,
    /// Management connected — the same signal the shell script used.
    pub connected: bool,
    /// Error / daemon state worth surfacing (management error, NeedsLogin, ...).
    pub note: Option<String>,
    pub cli_version: String,
    pub daemon_version: String,
    pub profile: Option<String>,
    pub ip: String,
    pub fqdn: String,
    pub management_url: String,
    pub signal_connected: bool,
    pub peers_connected: u64,
    pub peers_total: u64,
    pub networks: Vec<String>,
}

impl Status {
    fn unreachable(note: String) -> Self {
        Self {
            note: Some(note),
            ..Self::default()
        }
    }

    pub(crate) fn from_json(json: &Value) -> Self {
        let str_at = |ptr: &str| {
            json.pointer(ptr)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        let bool_at = |ptr: &str| json.pointer(ptr).and_then(Value::as_bool).unwrap_or(false);
        let num_at = |ptr: &str| json.pointer(ptr).and_then(Value::as_u64).unwrap_or(0);
        let non_empty = |s: String| Some(s).filter(|s| !s.is_empty());

        Self {
            reachable: true,
            connected: bool_at("/management/connected"),
            note: non_empty(str_at("/management/error")),
            cli_version: str_at("/cliVersion"),
            daemon_version: str_at("/daemonVersion"),
            profile: non_empty(str_at("/profileName")),
            ip: str_at("/netbirdIp"),
            fqdn: str_at("/fqdn"),
            management_url: str_at("/management/url"),
            signal_connected: bool_at("/signal/connected"),
            peers_connected: num_at("/peers/connected"),
            peers_total: num_at("/peers/total"),
            networks: json
                .pointer("/networks")
                .and_then(Value::as_array)
                .map(|nets| {
                    nets.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
        }
    }

    fn from_text(text: &str) -> Self {
        let connected = text.lines().any(|line| {
            let line = line.trim().to_ascii_lowercase();
            line.strip_prefix("management:")
                .is_some_and(|rest| rest.trim_start().starts_with("connected"))
        });
        let note = (!connected)
            .then(|| text.lines().map(str::trim).find(|l| !l.is_empty()))
            .flatten()
            .map(str::to_owned);
        Self {
            reachable: true,
            connected,
            note,
            ..Self::default()
        }
    }
}

// ---------------------------------------------------------------------------
// Profiles
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile {
    pub name: String,
    pub active: bool,
}

pub fn active_profile(profiles: &[Profile]) -> Option<String> {
    profiles.iter().find(|p| p.active).map(|p| p.name.clone())
}

/// `netbird profile list` prints a header line, then one profile per line with
/// a ✓ (active) or ✗ marker next to the name.
pub fn parse_profiles(text: &str) -> Vec<Profile> {
    let has_marker = |line: &str| line.contains(['✓', '✗']);
    let mut lines = text.lines().peekable();
    if lines.peek().is_some_and(|line| !has_marker(line)) {
        lines.next();
    }
    lines
        .filter_map(|line| {
            let cleaned = line.replace(['✓', '✗'], " ");
            let name = cleaned.split_whitespace().next()?.to_owned();
            Some(Profile {
                name,
                active: line.contains('✓'),
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Routes / networks
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Route {
    /// May contain spaces; always pass as a single argument.
    pub id: String,
    /// Network CIDR or comma-separated domains.
    pub target: String,
    pub selected: bool,
    /// Raw block from `routes list`, for the preview pane.
    pub details: Vec<String>,
}

/// Parses blocks like:
///
/// ```text
///   - ID: corp
///     Network: 10.0.0.0/16
///     Status: Selected
/// ```
pub fn parse_routes(text: &str) -> Vec<Route> {
    let mut routes: Vec<Route> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(id) = trimmed.strip_prefix("- ID:") {
            routes.push(Route {
                id: id.trim().to_owned(),
                target: String::new(),
                selected: false,
                details: vec![format!("ID: {}", id.trim())],
            });
            continue;
        }
        let Some(route) = routes.last_mut() else {
            continue;
        };
        if trimmed.is_empty() {
            continue;
        }
        route
            .details
            .push(line.strip_prefix("    ").unwrap_or(trimmed).trim_end().to_owned());
        if let Some((key, value)) = trimmed.split_once(':') {
            let value = value.trim();
            match key {
                "Network" | "Domains" if route.target.is_empty() => route.target = value.to_owned(),
                "Status" => route.selected = value == "Selected",
                _ => {}
            }
        }
    }
    routes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_with_header_and_markers() {
        let text = "Found 3 profiles:\n✓ default\n✗ work\n✗ home\n";
        let profiles = parse_profiles(text);
        assert_eq!(
            profiles,
            vec![
                Profile { name: "default".into(), active: true },
                Profile { name: "work".into(), active: false },
                Profile { name: "home".into(), active: false },
            ]
        );
        assert_eq!(active_profile(&profiles).as_deref(), Some("default"));
    }

    #[test]
    fn profiles_marker_after_name_and_no_header() {
        let text = "work ✓\nhome\n";
        let profiles = parse_profiles(text);
        assert_eq!(profiles.len(), 2);
        assert!(profiles[0].active);
        assert_eq!(profiles[1].name, "home");
    }

    #[test]
    fn routes_blocks() {
        let text = "Available Networks:\n\n  - ID: corp\n    Network: 10.0.0.0/16\n    Status: Selected\n\n  - ID: my domains\n    Domains: example.com, *.corp.example\n    Status: Not Selected\n    Resolved IPs:\n      [example.com]: 1.2.3.4\n\n  - ID: v6\n    Network: fd00::/64\n    Status: Selected\n";
        let routes = parse_routes(text);
        assert_eq!(routes.len(), 3);
        assert_eq!(routes[0].id, "corp");
        assert_eq!(routes[0].target, "10.0.0.0/16");
        assert!(routes[0].selected);
        assert_eq!(routes[1].id, "my domains");
        assert_eq!(routes[1].target, "example.com, *.corp.example");
        assert!(!routes[1].selected);
        assert_eq!(routes[1].details.last().unwrap(), "  [example.com]: 1.2.3.4");
        assert_eq!(routes[2].target, "fd00::/64");
    }

    #[test]
    fn status_json() {
        let json: Value = serde_json::from_str(
            r#"{"peers":{"total":5,"connected":3},"cliVersion":"0.83.2","daemonVersion":"0.83.2",
                "management":{"url":"https://api.netbird.io:443","connected":true,"error":""},
                "signal":{"connected":true},"netbirdIp":"100.92.0.1/16","fqdn":"me.netbird.cloud",
                "networks":["10.0.0.0/16"],"profileName":"work"}"#,
        )
        .unwrap();
        let status = Status::from_json(&json);
        assert!(status.reachable && status.connected && status.signal_connected);
        assert_eq!(status.profile.as_deref(), Some("work"));
        assert_eq!((status.peers_connected, status.peers_total), (3, 5));
        assert_eq!(status.note, None);
    }

    #[test]
    fn status_text() {
        assert!(Status::from_text("Daemon version: 0.83.2\nManagement: Connected\n").connected);
        let s = Status::from_text("Daemon status: NeedsLogin\n\nRun UP command\n");
        assert!(!s.connected);
        assert_eq!(s.note.as_deref(), Some("Daemon status: NeedsLogin"));
        assert!(!Status::from_text("Management: Disconnected\n").connected);
    }
}
