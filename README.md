# 🐦 nbi

Ratatui TUI for the [NetBird](https://netbird.io) client — a port of [`netbird-tui.sh`](https://github.com/coko7/scripts/blob/main/global/netbird-tui.sh).

- connect / disconnect (`netbird up` / `netbird down`)
- switch profile (reconnects if you were connected)
- tick/untick network routes, then apply
- full `netbird status --detail`, scrollable
- preview pane, fuzzy `/` filter, live command output (SSO login URLs show up there)

## Install

```sh
cargo install --git https://github.com/coko7/nbi
```

Or from a local checkout: `cargo install --path .`

## Usage

```sh
nbi        # TUI
nbi -x     # TUI that quits after a successful action (launcher style)
```

Keys: `↑↓`/`jk` move · `enter` select · `/` filter · `space`/`tab` toggle route ·
`a`/`n` all/none · `r` refresh · `esc` back · `q` quit.

In the status detail pane: `ctrl+u`/`ctrl+d` half page · `g`/`G` top/bottom ·
`y` copy the full output (via `wl-copy`, `xclip` or `xsel`, else an OSC 52
terminal escape).

## Waybar

`nbi waybar` prints one JSON line: `text` is the active profile while connected
(empty otherwise, which hides the module), `class`/`alt` is `connected`,
`disconnected` or `unavailable`, `tooltip` has the IP/FQDN/peers.
`nbi active` prints just the profile name and exits 1 when disconnected.

```jsonc
"custom/netbird": {
  "exec": "nbi waybar",
  "return-type": "json",
  "interval": 30,
  "signal": 8,            // nbi sends SIGRTMIN+8 to waybar after each change
  "format": "󰖂 {}",
  "on-click": "kitty --class floating -e nbi -x"
}
```

Pass `--disconnected-text 'off'` to keep the module visible while disconnected.

## Environment

| variable            | default         | purpose                                                                    |
| ------------------- | --------------- | -------------------------------------------------------------------------- |
| `NB`                | `netbird`       | netbird command; `sudo -n` is added automatically if the daemon refuses you |
| `NBI_NOTIFY_CMD`    | `ntfy-toast.sh` | toast command (falls back to `notify-send`); empty disables toasts         |
| `NBI_WAYBAR_SIGNAL` | `8`             | `pkill -RTMIN+N waybar` after changes; `0` disables                        |
