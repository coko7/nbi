mod app;
mod integrations;
mod netbird;
mod ui;

use std::process::ExitCode;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::app::App;
use crate::netbird::Netbird;

/// Terminal UI for the NetBird client: connect/disconnect, switch profiles,
/// pick network routes, inspect status.
///
/// Environment:
///   NB                 netbird command to run (default: netbird; sudo -n is added
///                      automatically when the daemon refuses the current user)
///   NBI_NOTIFY_CMD     toast command (default: ntfy-toast.sh, falls back to
///                      notify-send); set empty to disable notifications
///   NBI_WAYBAR_SIGNAL  RTMIN offset sent to waybar after changes (default: 8, 0 disables)
#[derive(Parser)]
#[command(name = "nbi", version, verbatim_doc_comment)]
struct Cli {
    /// Quit the TUI once an action succeeds (handy from a launcher / floating terminal)
    #[arg(short = 'x', long)]
    exit_on_action: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Print the active profile if connected; print nothing and exit 1 otherwise
    Active,
    /// Print one JSON line for a waybar custom module ("return-type": "json")
    ///
    /// text = active profile while connected, class/alt = connected |
    /// disconnected | unavailable, tooltip = ip/fqdn/peers.
    Waybar {
        /// Text shown while not connected (empty hides the module)
        #[arg(long, default_value = "")]
        disconnected_text: String,
    },
}

fn main() -> Result<ExitCode> {
    let cli = Cli::parse();
    let nb = Netbird::detect();

    match cli.command {
        Some(Command::Active) => {
            let status = nb.status();
            if !status.connected {
                return Ok(ExitCode::FAILURE);
            }
            println!("{}", nb.resolve_profile(&status));
        }
        Some(Command::Waybar { disconnected_text }) => {
            let status = nb.status();
            println!("{}", integrations::waybar_json(&nb, &status, &disconnected_text));
        }
        None => {
            let mut terminal = ratatui::init();
            let result = App::new(nb, cli.exit_on_action).run(&mut terminal);
            ratatui::restore();
            if let Some(message) = result? {
                println!("{message}");
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}
