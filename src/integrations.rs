//! Desktop glue: toasts, waybar refresh signal, waybar JSON output.
//! Everything shells out — no extra crates, failures are silently ignored.

use std::env;
use std::io;
use std::process::{Command, Stdio};
use std::thread;

use serde_json::json;

use crate::netbird::{Netbird, Status};

/// Toast via `$NBI_NOTIFY_CMD` (default `ntfy-toast.sh`, same arguments as the
/// shell script), falling back to `notify-send`. Set `NBI_NOTIFY_CMD=` to disable.
pub fn notify(message: &str, sound: &str) {
    let custom = env::var("NBI_NOTIFY_CMD").ok();
    if custom.as_deref() == Some("") {
        return;
    }
    let program = custom.unwrap_or_else(|| "ntfy-toast.sh".to_owned());
    let sound = format!("/usr/share/sounds/freedesktop/stereo/{sound}.oga");
    let spawned = spawn_quiet(Command::new(&program).args([
        "Netbird",
        message,
        "netbird.png",
        "ntfy-netbird-sig-down",
        &sound,
    ]));
    if matches!(spawned, Err(ref e) if e.kind() == io::ErrorKind::NotFound) {
        let _ = spawn_quiet(Command::new("notify-send").args(["--app-name=NetBird", "NetBird", message]));
    }
}

pub fn notify_connection(profile: &str, up: bool) {
    if up {
        notify(&format!("Connected to Netbird — \"{profile}\" 👋"), "service-login");
    } else {
        notify(&format!("Disconnected from Netbird — \"{profile}\" 👋"), "service-logout");
    }
}

/// `pkill -RTMIN+$NBI_WAYBAR_SIGNAL waybar` (default 8) so the module refreshes
/// immediately. `NBI_WAYBAR_SIGNAL=0` disables it.
pub fn signal_waybar() {
    let signal = env::var("NBI_WAYBAR_SIGNAL").unwrap_or_else(|_| "8".to_owned());
    if signal.is_empty() || signal == "0" {
        return;
    }
    let _ = Command::new("pkill")
        .arg(format!("-RTMIN+{signal}"))
        .arg("waybar")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Spawn without waiting on the caller's thread; a helper thread reaps the child.
/// Spawning happens synchronously so the toast still fires if nbi exits right after.
fn spawn_quiet(cmd: &mut Command) -> io::Result<()> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// One JSON line for a waybar `custom` module with `"return-type": "json"`.
/// `text` is the active profile while connected; otherwise `disconnected_text`
/// (empty by default, which makes waybar hide the module).
pub fn waybar_json(nb: &Netbird, status: &Status, disconnected_text: &str) -> String {
    let profile = nb.resolve_profile(status);
    let (text, class) = match (status.reachable, status.connected) {
        (_, true) => (profile.as_str(), "connected"),
        (true, false) => (disconnected_text, "disconnected"),
        (false, _) => (disconnected_text, "unavailable"),
    };

    let mut tooltip = match class {
        "connected" => format!("NetBird connected\nprofile: {profile}"),
        "disconnected" => format!("NetBird disconnected\nprofile: {profile}"),
        _ => "NetBird daemon unreachable".to_owned(),
    };
    if status.connected {
        if !status.ip.is_empty() {
            tooltip.push_str(&format!("\nip: {}", status.ip));
        }
        if !status.fqdn.is_empty() {
            tooltip.push_str(&format!("\nfqdn: {}", status.fqdn));
        }
        tooltip.push_str(&format!(
            "\npeers: {}/{}",
            status.peers_connected, status.peers_total
        ));
    }
    if let Some(note) = &status.note {
        tooltip.push_str(&format!("\n{note}"));
    }

    json!({
        "text": pango_escape(text),
        "alt": class,
        "class": class,
        "tooltip": pango_escape(&tooltip),
    })
    .to_string()
}

/// Waybar renders text/tooltip as Pango markup.
fn pango_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}
