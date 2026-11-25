//! Rendering. Layout mirrors the fzf version: list on the left, preview on
//! the right, plus a status header, a command output pane and key hints.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, List, ListItem, Paragraph, Wrap};

use crate::app::{App, MenuItem, View};
use crate::netbird::Status;

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const LOG_MAX_HEIGHT: u16 = 10;

pub fn draw(frame: &mut Frame, app: &mut App) {
    let log_height = match app.log.len() {
        0 => 0,
        n => (n as u16).saturating_add(2).min(LOG_MAX_HEIGHT),
    };
    let [header, body, log, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(6),
        Constraint::Length(log_height),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    draw_header(frame, header, app);
    if app.view == View::Detail {
        draw_detail(frame, body, app);
    } else {
        let [list, preview] =
            Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)]).areas(body);
        draw_list(frame, list, app);
        draw_preview(frame, preview, app);
    }
    if log_height > 0 {
        draw_log(frame, log, app);
    }
    draw_footer(frame, footer, app);
}

fn block(title: impl Into<Line<'static>>) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .title(title.into())
}

// ---------------------------------------------------------------------------
// Header
// ---------------------------------------------------------------------------

fn draw_header(frame: &mut Frame, area: Rect, app: &App) {
    let sep = || Span::raw("  ·  ").dark_gray();
    let mut spans: Vec<Span> = Vec::new();
    match &app.status {
        None => spans.push("loading…".dark_gray()),
        Some(status) if !status.reachable => {
            spans.push("✖ daemon unreachable".red().bold());
            if let Some(note) = &status.note {
                spans.push(sep());
                spans.push(Span::raw(note.clone()).dark_gray());
            }
        }
        Some(status) => {
            spans.push(if status.connected {
                "● connected".green().bold()
            } else {
                "○ disconnected".red().bold()
            });
            spans.push(sep());
            spans.push("profile ".dark_gray());
            spans.push(Span::raw(app.active_profile()).bold());
            if status.connected {
                if !status.ip.is_empty() {
                    spans.push(sep());
                    spans.push(Span::raw(status.ip.clone()));
                }
                spans.push(sep());
                spans.push(Span::raw(format!(
                    "{}/{} peers",
                    status.peers_connected, status.peers_total
                )));
            } else if let Some(note) = &status.note {
                spans.push(sep());
                spans.push(Span::raw(note.clone()).yellow());
            }
        }
    }

    let version = app
        .status
        .as_ref()
        .map(|s| s.cli_version.clone())
        .filter(|v| !v.is_empty())
        .or_else(|| app.cli_version.clone())
        .unwrap_or_else(|| "…".to_owned());
    let header = block(Line::from(" nbi ".bold()))
        .title_top(Line::from(format!(" netbird {version} ")).right_aligned());
    frame.render_widget(Paragraph::new(Line::from(spans)).block(header), area);
}

// ---------------------------------------------------------------------------
// Left: list
// ---------------------------------------------------------------------------

fn prompt(name: &str, app: &App) -> Line<'static> {
    let mut spans = vec![Span::raw(format!(" {name} > ")).bold(), Span::raw(app.filter.clone())];
    if app.filtering {
        spans.push("█".slow_blink());
    }
    spans.push(Span::raw(" "));
    Line::from(spans)
}

fn draw_list(frame: &mut Frame, area: Rect, app: &mut App) {
    let visible = app.visible();
    let (title, items, empty): (Line, Vec<ListItem>, Option<Text>) = match app.view {
        View::Menu => (
            Line::from(" netbird > ".bold()),
            MenuItem::ALL.iter().map(|item| ListItem::new(menu_label(app, *item))).collect(),
            None,
        ),
        View::Profiles => {
            let profiles = app.profile_list();
            let items = visible.iter().map(|&i| profile_line(&profiles[i].name, profiles[i].active)).map(ListItem::new).collect();
            let empty = match &app.profiles {
                None => Some(Text::from("loading…".dark_gray())),
                Some(Err(err)) => Some(error_text(err, "profile support requires netbird >= 0.52")),
                Some(Ok(_)) => Some(Text::from("no match".dark_gray())),
            };
            (prompt("profile", app), items, empty)
        }
        View::Routes => {
            let routes = app.route_list();
            let width = visible.iter().map(|&i| routes[i].id.chars().count()).max().unwrap_or(0).min(28);
            let items = visible
                .iter()
                .map(|&i| {
                    let route = &routes[i];
                    let checked = app.checked.contains(&route.id);
                    let pending = checked != route.selected;
                    let mut spans = vec![
                        if checked { "[x] ".green().bold() } else { "[ ] ".dark_gray() },
                        Span::raw(format!("{:<width$}  ", route.id)),
                        Span::raw(route.target.clone()).dark_gray(),
                    ];
                    if pending {
                        spans.push(" *".yellow().bold());
                    }
                    ListItem::new(Line::from(spans))
                })
                .collect();
            let empty = match &app.routes {
                _ if !app.connected() => Some(Text::from("not connected — connect to netbird to see routes".dark_gray())),
                None => Some(Text::from("loading…".dark_gray())),
                Some(Err(err)) => Some(error_text(err, "route support requires netbird >= 0.36")),
                Some(Ok(all)) if all.is_empty() => Some(Text::from("no routes available".dark_gray())),
                Some(Ok(_)) => Some(Text::from("no match".dark_gray())),
            };
            let mut title = prompt("routes", app);
            title.push_span(Span::raw(format!("{}/{} ", app.checked.len(), routes.len())).dark_gray());
            (title, items, empty)
        }
        View::Detail => return,
    };

    let list_block = block(title);
    if items.is_empty()
        && let Some(empty) = empty
    {
        frame.render_widget(Paragraph::new(empty).wrap(Wrap { trim: false }).block(list_block), area);
        return;
    }
    let list = List::new(items)
        .block(list_block)
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED))
        .highlight_symbol("▌");
    frame.render_stateful_widget(list, area, app.list_state_mut());
}

fn menu_label(app: &App, item: MenuItem) -> Line<'static> {
    let (name, hint) = match item {
        MenuItem::Toggle if app.connected() => ("disconnect", "(netbird down)"),
        MenuItem::Toggle => ("connect", "(netbird up)"),
        MenuItem::Profile => ("profile", "(switch profile)"),
        MenuItem::Routes => ("routes", "(select network routes)"),
        MenuItem::Status => ("status", "(detailed status)"),
    };
    Line::from(vec![Span::raw(format!("{name:<12}")).bold(), hint.dark_gray()])
}

fn profile_line(name: &str, active: bool) -> Line<'static> {
    if active {
        Line::from(vec!["✓ ".green().bold(), Span::raw(name.to_owned()).green().bold()])
    } else {
        Line::from(vec![Span::raw("  "), Span::raw(name.to_owned())])
    }
}

fn error_text(err: &str, hint: &str) -> Text<'static> {
    Text::from(vec![
        Line::from(Span::raw(err.to_owned()).red()),
        Line::from(Span::raw(format!("({hint})")).dark_gray()),
    ])
}

// ---------------------------------------------------------------------------
// Right: preview
// ---------------------------------------------------------------------------

fn draw_preview(frame: &mut Frame, area: Rect, app: &App) {
    let lines = match app.view {
        View::Menu => menu_preview(app),
        View::Profiles => profile_preview(app),
        View::Routes => route_preview(app),
        View::Detail => Vec::new(),
    };
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(block(" preview ")),
        area,
    );
}

fn kv(key: &str, value: impl Into<String>) -> Line<'static> {
    Line::from(vec![Span::raw(format!("{key:<11}")).dark_gray(), Span::raw(value.into())])
}

fn state_word(on: bool) -> Span<'static> {
    if on { "connected".green() } else { "disconnected".red() }
}

fn status_summary(status: &Status) -> Vec<Line<'static>> {
    if !status.reachable {
        return vec![
            Line::from("daemon not reachable".red().bold()),
            Line::from(Span::raw(status.note.clone().unwrap_or_default()).dark_gray()),
        ];
    }
    let mut lines = vec![
        kv("daemon", status.daemon_version.clone()),
        Line::from(vec![
            Span::raw(format!("{:<11}", "management")).dark_gray(),
            state_word(status.connected),
            Span::raw(format!("  {}", status.management_url)).dark_gray(),
        ]),
        Line::from(vec![Span::raw(format!("{:<11}", "signal")).dark_gray(), state_word(status.signal_connected)]),
    ];
    if status.connected {
        lines.push(kv("ip", status.ip.clone()));
        lines.push(kv("fqdn", status.fqdn.clone()));
        lines.push(kv("peers", format!("{}/{} connected", status.peers_connected, status.peers_total)));
        if !status.networks.is_empty() {
            lines.push(kv("networks", status.networks.join(", ")));
        }
    }
    if let Some(note) = &status.note {
        lines.push(Line::default());
        lines.push(Line::from(Span::raw(note.clone()).yellow()));
    }
    lines
}

fn menu_preview(app: &App) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    match app.current_menu_item() {
        MenuItem::Toggle => {
            let (cmd, what) = if app.connected() {
                ("netbird down", "Disconnect from")
            } else {
                ("netbird up", "Connect to")
            };
            lines.push(Line::from(vec![Span::raw("runs "), Span::raw(cmd).bold().cyan()]));
            lines.push(Line::from(format!("{what} NetBird using profile \"{}\"", app.active_profile())));
            lines.push(Line::default());
            if let Some(status) = &app.status {
                lines.extend(status_summary(status));
            }
        }
        MenuItem::Profile => {
            lines.push(Line::from("Available profiles:".bold()));
            match &app.profiles {
                None => lines.push(Line::from("loading…".dark_gray())),
                Some(Err(err)) => lines.extend(error_text(err, "profile support requires netbird >= 0.52").lines),
                Some(Ok(profiles)) => lines.extend(profiles.iter().map(|p| profile_line(&p.name, p.active))),
            }
        }
        MenuItem::Routes => {
            if !app.connected() {
                lines.push(Line::from("not connected — connect to netbird to see routes".dark_gray()));
            } else {
                lines.push(Line::from("Available routes:".bold()));
                match &app.routes {
                    None => lines.push(Line::from("loading…".dark_gray())),
                    Some(Err(err)) => lines.extend(error_text(err, "route support requires netbird >= 0.36").lines),
                    Some(Ok(routes)) if routes.is_empty() => lines.push(Line::from("none".dark_gray())),
                    Some(Ok(routes)) => lines.extend(routes.iter().map(|r| {
                        Line::from(vec![
                            if r.selected { "✓ ".green() } else { "  ".into() },
                            Span::raw(r.id.clone()),
                            Span::raw(format!("  {}", r.target)).dark_gray(),
                        ])
                    })),
                }
            }
        }
        MenuItem::Status => {
            match &app.status {
                Some(status) => lines.extend(status_summary(status)),
                None => lines.push(Line::from("loading…".dark_gray())),
            }
            lines.push(Line::default());
            lines.push(Line::from(vec![
                "enter".bold(),
                " → full ".dark_gray(),
                "netbird status --detail".cyan(),
            ]));
        }
    }
    lines
}

fn profile_preview(app: &App) -> Vec<Line<'static>> {
    let Some(profile) = app.current_profile() else {
        return Vec::new();
    };
    let mut lines = vec![
        kv("profile", profile.name.clone()).bold(),
        kv("active", if profile.active { "yes" } else { "no" }),
        Line::default(),
    ];
    if profile.active {
        lines.push(Line::from("already the active profile".dark_gray()));
    } else if app.connected() {
        lines.push(Line::from(vec!["enter".bold(), " → switch and reconnect".dark_gray()]));
        lines.push(Line::from("netbird down → profile select → netbird up".cyan()));
    } else {
        lines.push(Line::from(vec!["enter".bold(), " → switch (stays disconnected)".dark_gray()]));
        lines.push(Line::from(format!("netbird profile select {}", profile.name).cyan()));
    }
    lines
}

fn route_preview(app: &App) -> Vec<Line<'static>> {
    let Some(route) = app.current_route() else {
        return Vec::new();
    };
    let mut lines: Vec<Line> = route
        .details
        .iter()
        .map(|line| match line.split_once(':') {
            Some(("Status", value)) => Line::from(vec![
                "Status:".dark_gray(),
                if route.selected { Span::raw(value.to_owned()).green() } else { Span::raw(value.to_owned()).red() },
            ]),
            Some((key, value)) if !key.starts_with(' ') => Line::from(vec![
                Span::raw(format!("{key}:")).dark_gray(),
                Span::raw(value.to_owned()),
            ]),
            _ => Line::from(line.clone()),
        })
        .collect();
    let checked = app.checked.contains(&route.id);
    if checked != route.selected {
        lines.push(Line::default());
        lines.push(Line::from(
            if checked { "pending: will be selected" } else { "pending: will be deselected" }.yellow(),
        ));
    }
    lines
}

// ---------------------------------------------------------------------------
// Full-screen detail
// ---------------------------------------------------------------------------

fn draw_detail(frame: &mut Frame, area: Rect, app: &mut App) {
    app.detail_height = area.height.saturating_sub(2);
    let text = match &app.detail {
        None => Text::from("loading…".dark_gray()),
        Some(Ok(text)) => Text::from(text.clone()),
        Some(Err(err)) => Text::from(Span::raw(err.clone()).red()),
    };
    let total = app.detail_lines();
    let mut detail_block = block(" netbird status --detail ");
    if total > 0 {
        let first = usize::from(app.detail_scroll) + 1;
        let last = (first + usize::from(app.detail_height)).saturating_sub(1).min(total);
        detail_block = detail_block
            .title_top(Line::from(format!(" {first}-{last}/{total} ").dark_gray()).right_aligned());
    }
    frame.render_widget(
        Paragraph::new(text)
            .scroll((app.detail_scroll, 0))
            .block(detail_block),
        area,
    );
}

// ---------------------------------------------------------------------------
// Bottom: command output + key hints
// ---------------------------------------------------------------------------

fn draw_log(frame: &mut Frame, area: Rect, app: &App) {
    let width = usize::from(area.width.saturating_sub(2)).max(1);
    let height = usize::from(area.height.saturating_sub(2));
    // hard-wrap ourselves so long lines (SSO URLs!) stay visible and we can tail
    let wrapped: Vec<Line> = app
        .log
        .iter()
        .flat_map(|line| {
            let style = if line.starts_with("$ ") {
                Style::new().bold().cyan()
            } else if line.starts_with('✔') {
                Style::new().green()
            } else if line.starts_with('✖') || line.starts_with("Error") || line.starts_with("error") {
                Style::new().red()
            } else {
                Style::new()
            };
            let chars: Vec<char> = line.chars().collect();
            chars
                .chunks(width)
                .map(|chunk| Line::styled(chunk.iter().collect::<String>(), style))
                .collect::<Vec<_>>()
        })
        .collect();
    let start = wrapped.len().saturating_sub(height);
    frame.render_widget(
        Paragraph::new(wrapped[start..].to_vec()).block(block(" output ")),
        area,
    );
}

fn hints(pairs: &[(&'static str, &'static str)]) -> Line<'static> {
    let mut spans = Vec::new();
    for (key, desc) in pairs {
        spans.push(Span::raw(format!(" {key}")).bold());
        spans.push(Span::raw(format!(" {desc} ")).dark_gray());
    }
    Line::from(spans)
}

fn draw_footer(frame: &mut Frame, area: Rect, app: &App) {
    let keys = match (app.view, app.filtering) {
        (View::Profiles | View::Routes, true) => {
            hints(&[("type", "filter"), ("↑↓", "move"), ("enter", "accept"), ("esc", "clear")])
        }
        (View::Menu, _) => hints(&[("↑↓/jk", "move"), ("enter", "select"), ("r", "refresh"), ("q", "quit")]),
        (View::Profiles, _) => hints(&[("↑↓/jk", "move"), ("/", "filter"), ("enter", "switch"), ("esc", "back")]),
        (View::Routes, _) => hints(&[
            ("↑↓/jk", "move"),
            ("space", "toggle"),
            ("a/n", "all/none"),
            ("/", "filter"),
            ("enter", "apply"),
            ("esc", "back"),
        ]),
        (View::Detail, _) => hints(&[
            ("↑↓/jk", "scroll"),
            ("^u/^d", "half page"),
            ("g/G", "top/bottom"),
            ("y", "copy"),
            ("r", "reload"),
            ("esc", "back"),
        ]),
    };
    frame.render_widget(Paragraph::new(keys), area);

    let right = if let Some(busy) = &app.busy {
        Line::from(format!("{} {busy}… ", SPINNER[app.spinner % SPINNER.len()]).yellow())
    } else if let Some((msg, _)) = &app.flash {
        Line::from(format!("{msg} ").yellow())
    } else {
        Line::default()
    };
    frame.render_widget(Paragraph::new(right.right_aligned()), area);
}
