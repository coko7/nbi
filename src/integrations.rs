//! Desktop glue: toasts, waybar refresh signal, waybar JSON output.
//! Everything shells out — no extra crates, failures are silently ignored.

use std::env;
use std::io::{self, Write};
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

/// Copy to the system clipboard via wl-copy / xclip / xsel (whichever fits the
/// session and is installed), else an OSC 52 escape that the terminal itself
/// handles (kitty, ghostty, foot, wezterm, alacritty...). Returns the method used.
pub fn copy_to_clipboard(text: &str) -> &'static str {
    let wayland = env::var_os("WAYLAND_DISPLAY").is_some();
    let x11 = env::var_os("DISPLAY").is_some();
    let tools: [(&'static str, &[&str], bool); 3] = [
        ("wl-copy", &[], wayland),
        ("xclip", &["-selection", "clipboard"], x11),
        ("xsel", &["--clipboard", "--input"], x11),
    ];
    for (program, args, _) in tools.into_iter().filter(|(_, _, usable)| *usable) {
        if pipe_into(program, args, text).is_ok() {
            return program;
        }
    }
    let mut stdout = io::stdout().lock();
    let _ = write!(stdout, "\x1b]52;c;{}\x07", base64(text.as_bytes()));
    let _ = stdout.flush();
    "OSC 52"
}

fn pipe_into(program: &str, args: &[&str], text: &str) -> io::Result<()> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    // dropping stdin right after the write sends EOF
    let written = child.stdin.take().map_or(Ok(()), |mut stdin| stdin.write_all(text.as_bytes()));
    let status = child.wait()?;
    written?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("{program} exited with {status}")))
    }
}

fn base64(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let bytes = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(bytes[0]) << 16) | (u32::from(bytes[1]) << 8) | u32::from(bytes[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(TABLE[((n >> (18 - 6 * i)) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
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

#[cfg(test)]
mod tests {
    use super::base64;

    #[test]
    fn base64_padding() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64("hello ✓".as_bytes()), "aGVsbG8g4pyT");
    }
}
