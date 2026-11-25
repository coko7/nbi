//! TUI state + event handling. All netbird calls run on worker threads and
//! report back through an mpsc channel, so the UI never blocks.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Read};
use std::process::Stdio;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::widgets::ListState;

use crate::integrations::{notify, notify_connection, signal_waybar};
use crate::netbird::{self, Netbird, Profile, Route, Status};

const REFRESH_EVERY: Duration = Duration::from_secs(5);
const FLASH_FOR: Duration = Duration::from_secs(3);
const LOG_CAPACITY: usize = 500;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    Menu,
    Profiles,
    Routes,
    Detail,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuItem {
    Toggle,
    Profile,
    Routes,
    Status,
}

impl MenuItem {
    pub const ALL: [MenuItem; 4] = [Self::Toggle, Self::Profile, Self::Routes, Self::Status];
}

pub enum Msg {
    Status(Status),
    Version(String),
    Profiles(Result<Vec<Profile>, String>),
    Routes(Result<Vec<Route>, String>),
    Detail(Result<String, String>),
    Log(String),
    Done { ok: bool, summary: String },
}

/// `None` = still loading.
pub type Loaded<T> = Option<Result<T, String>>;

pub struct App {
    nb: Netbird,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    exit_on_action: bool,
    quit: bool,
    exit_message: Option<String>,

    pub view: View,
    pub status: Option<Status>,
    pub cli_version: Option<String>,
    pub profiles: Loaded<Vec<Profile>>,
    pub routes: Loaded<Vec<Route>>,
    /// Route IDs ticked in the routes view (pending selection).
    pub checked: HashSet<String>,
    routes_touched: bool,
    pub detail: Loaded<String>,
    pub detail_scroll: u16,

    pub menu_state: ListState,
    pub profile_state: ListState,
    pub route_state: ListState,
    pub filter: String,
    pub filtering: bool,

    pub log: Vec<String>,
    pub busy: Option<String>,
    pub flash: Option<(String, Instant)>,
    pub spinner: usize,
    status_inflight: bool,
    last_refresh: Instant,
}

impl App {
    pub fn new(nb: Netbird, exit_on_action: bool) -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            nb,
            tx,
            rx,
            exit_on_action,
            quit: false,
            exit_message: None,
            view: View::Menu,
            status: None,
            cli_version: None,
            profiles: None,
            routes: None,
            checked: HashSet::new(),
            routes_touched: false,
            detail: None,
            detail_scroll: 0,
            menu_state: ListState::default().with_selected(Some(0)),
            profile_state: ListState::default(),
            route_state: ListState::default(),
            filter: String::new(),
            filtering: false,
            log: Vec::new(),
            busy: None,
            flash: None,
            spinner: 0,
            status_inflight: false,
            last_refresh: Instant::now(),
        }
    }

    /// Runs until quit; returns a message to print after the terminal is restored.
    pub fn run(mut self, terminal: &mut DefaultTerminal) -> Result<Option<String>> {
        self.spawn(|nb| Msg::Version(nb.version()));
        self.refresh_all();
        while !self.quit {
            terminal.draw(|frame| crate::ui::draw(frame, &mut self))?;
            if event::poll(Duration::from_millis(100))?
                && let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
            {
                self.on_key(key);
            }
            while let Ok(msg) = self.rx.try_recv() {
                self.on_msg(msg);
            }
            self.on_tick();
        }
        Ok(self.exit_message)
    }

    // -----------------------------------------------------------------------
    // Derived state
    // -----------------------------------------------------------------------

    pub fn connected(&self) -> bool {
        self.status.as_ref().is_some_and(|s| s.connected)
    }

    pub fn active_profile(&self) -> String {
        self.status
            .as_ref()
            .and_then(|s| s.profile.clone())
            .or_else(|| netbird::active_profile(self.profile_list()))
            .unwrap_or_else(|| "default".to_owned())
    }

    pub fn profile_list(&self) -> &[Profile] {
        match &self.profiles {
            Some(Ok(profiles)) => profiles,
            _ => &[],
        }
    }

    pub fn route_list(&self) -> &[Route] {
        match &self.routes {
            Some(Ok(routes)) => routes,
            _ => &[],
        }
    }

    /// Indices (into the underlying list) that match the current filter.
    pub fn visible(&self) -> Vec<usize> {
        match self.view {
            View::Profiles => matching(self.profile_list(), |p| fuzzy_match(&self.filter, &p.name)),
            View::Routes => matching(self.route_list(), |r| {
                fuzzy_match(&self.filter, &r.id) || fuzzy_match(&self.filter, &r.target)
            }),
            View::Menu | View::Detail => (0..MenuItem::ALL.len()).collect(),
        }
    }

    pub fn list_state_mut(&mut self) -> &mut ListState {
        match self.view {
            View::Profiles => &mut self.profile_state,
            View::Routes => &mut self.route_state,
            View::Menu | View::Detail => &mut self.menu_state,
        }
    }

    fn list_state(&self) -> &ListState {
        match self.view {
            View::Profiles => &self.profile_state,
            View::Routes => &self.route_state,
            View::Menu | View::Detail => &self.menu_state,
        }
    }

    /// Index into the underlying list of the highlighted row.
    pub fn current(&self) -> Option<usize> {
        let selected = self.list_state().selected()?;
        self.visible().get(selected).copied()
    }

    pub fn current_menu_item(&self) -> MenuItem {
        MenuItem::ALL[self.menu_state.selected().unwrap_or(0).min(MenuItem::ALL.len() - 1)]
    }

    pub fn current_profile(&self) -> Option<&Profile> {
        self.profile_list().get(self.current()?)
    }

    pub fn current_route(&self) -> Option<&Route> {
        self.route_list().get(self.current()?)
    }

    fn clamp_selection(&mut self) {
        let len = self.visible().len();
        let state = self.list_state_mut();
        let selected = match len {
            0 => None,
            _ => Some(state.selected().unwrap_or(0).min(len - 1)),
        };
        state.select(selected);
    }

    fn flash(&mut self, msg: impl Into<String>) {
        self.flash = Some((msg.into(), Instant::now()));
    }

    // -----------------------------------------------------------------------
    // Background work
    // -----------------------------------------------------------------------

    fn spawn(&self, job: impl FnOnce(&Netbird) -> Msg + Send + 'static) {
        let (nb, tx) = (self.nb.clone(), self.tx.clone());
        thread::spawn(move || {
            let _ = tx.send(job(&nb));
        });
    }

    fn refresh_status(&mut self, force: bool) {
        if self.status_inflight && !force {
            return;
        }
        self.status_inflight = true;
        self.last_refresh = Instant::now();
        self.spawn(|nb| Msg::Status(nb.status()));
    }

    fn load_profiles(&self) {
        self.spawn(|nb| Msg::Profiles(nb.profiles().map_err(|e| format!("{e:#}"))));
    }

    fn load_routes(&self) {
        self.spawn(|nb| Msg::Routes(nb.routes().map_err(|e| format!("{e:#}"))));
    }

    fn load_detail(&mut self) {
        self.detail = None;
        self.detail_scroll = 0;
        self.spawn(|nb| Msg::Detail(nb.detail().map_err(|e| format!("{e:#}"))));
    }

    fn refresh_all(&mut self) {
        self.refresh_status(true);
        self.load_profiles();
        self.load_routes();
        if self.view == View::Detail {
            self.load_detail();
        }
    }

    /// Start a state-changing job (one at a time). Returns false if another is running.
    fn start_job(
        &mut self,
        label: &str,
        job: impl FnOnce(&Netbird, &Sender<Msg>) -> (bool, String) + Send + 'static,
    ) -> bool {
        if let Some(busy) = &self.busy {
            self.flash(format!("busy: {busy}"));
            return false;
        }
        self.busy = Some(label.to_owned());
        self.log.clear();
        let (nb, tx) = (self.nb.clone(), self.tx.clone());
        thread::spawn(move || {
            let (ok, summary) = job(&nb, &tx);
            let _ = tx.send(Msg::Done { ok, summary });
        });
        true
    }

    fn on_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Status(status) => {
                self.status_inflight = false;
                let changed = status.connected != self.connected();
                self.status = Some(status);
                if changed {
                    self.load_routes();
                }
            }
            Msg::Version(version) => self.cli_version = Some(version),
            Msg::Profiles(profiles) => {
                self.profiles = Some(profiles);
                if self.view == View::Profiles {
                    self.clamp_selection();
                }
            }
            Msg::Routes(routes) => {
                self.routes = Some(routes);
                // don't clobber ticks the user is in the middle of editing
                if !(self.view == View::Routes && self.routes_touched) {
                    self.reset_checked();
                }
                if self.view == View::Routes {
                    self.clamp_selection();
                }
            }
            Msg::Detail(detail) => self.detail = Some(detail),
            Msg::Log(line) => {
                self.log.push(line);
                if self.log.len() > LOG_CAPACITY {
                    self.log.drain(..self.log.len() - LOG_CAPACITY);
                }
            }
            Msg::Done { ok, summary } => {
                self.busy = None;
                self.log.push(format!("{} {summary}", if ok { "✔" } else { "✖" }));
                self.refresh_status(true);
                self.load_profiles();
                self.load_routes();
                if ok && self.exit_on_action {
                    self.exit_message = Some(summary);
                    self.quit = true;
                }
            }
        }
    }

    fn on_tick(&mut self) {
        self.spinner = self.spinner.wrapping_add(1);
        if self.busy.is_none() && self.last_refresh.elapsed() >= REFRESH_EVERY {
            self.refresh_status(false);
        }
        if self.flash.as_ref().is_some_and(|(_, at)| at.elapsed() > FLASH_FOR) {
            self.flash = None;
        }
    }

    // -----------------------------------------------------------------------
    // Keys
    // -----------------------------------------------------------------------

    fn on_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        if self.filtering {
            self.on_filter_key(key);
            return;
        }
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('r') => {
                self.refresh_all();
                self.flash("refreshing…");
            }
            _ => match self.view {
                View::Menu => self.on_menu_key(key),
                View::Profiles | View::Routes => self.on_list_key(key),
                View::Detail => self.on_detail_key(key),
            },
        }
    }

    /// Cursor movement shared by all lists. Letter keys are off while typing a filter.
    fn navigate(&mut self, code: KeyCode, letters: bool) -> bool {
        let len = self.visible().len();
        if len == 0 {
            return matches!(code, KeyCode::Up | KeyCode::Down);
        }
        let last = len - 1;
        let state = self.list_state_mut();
        let cur = state.selected().unwrap_or(0);
        let next = match code {
            KeyCode::Up => cur.saturating_sub(1),
            KeyCode::Down => (cur + 1).min(last),
            KeyCode::PageUp => cur.saturating_sub(10),
            KeyCode::PageDown => (cur + 10).min(last),
            KeyCode::Home => 0,
            KeyCode::End => last,
            KeyCode::Char('k') if letters => cur.saturating_sub(1),
            KeyCode::Char('j') if letters => (cur + 1).min(last),
            KeyCode::Char('g') if letters => 0,
            KeyCode::Char('G') if letters => last,
            _ => return false,
        };
        state.select(Some(next));
        true
    }

    fn on_menu_key(&mut self, key: KeyEvent) {
        if self.navigate(key.code, true) {
            return;
        }
        match key.code {
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => self.activate_menu(),
            KeyCode::Esc => self.quit = true,
            _ => {}
        }
    }

    fn activate_menu(&mut self) {
        match self.current_menu_item() {
            MenuItem::Toggle => self.toggle_connection(),
            MenuItem::Profile => self.open_list(View::Profiles),
            MenuItem::Routes => self.open_list(View::Routes),
            MenuItem::Status => {
                self.view = View::Detail;
                self.load_detail();
            }
        }
    }

    fn open_list(&mut self, view: View) {
        self.view = view;
        self.filter.clear();
        self.filtering = false;
        match view {
            View::Profiles => {
                let active = self.active_profile();
                let idx = self.profile_list().iter().position(|p| p.name == active);
                self.profile_state.select(Some(idx.unwrap_or(0)));
                self.load_profiles();
            }
            View::Routes => {
                self.routes_touched = false;
                self.reset_checked();
                self.route_state.select(Some(0));
                self.load_routes();
            }
            View::Menu | View::Detail => {}
        }
        self.clamp_selection();
    }

    fn back(&mut self) {
        if self.filter.is_empty() {
            self.view = View::Menu;
        } else {
            self.filter.clear();
            self.clamp_selection();
        }
    }

    fn on_list_key(&mut self, key: KeyEvent) {
        if self.navigate(key.code, true) {
            return;
        }
        let routes = self.view == View::Routes;
        match key.code {
            KeyCode::Esc | KeyCode::Left | KeyCode::Char('h') => self.back(),
            KeyCode::Char('/') => self.filtering = true,
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => self.confirm_list(),
            KeyCode::Char(' ') | KeyCode::Tab if routes => self.toggle_route(),
            KeyCode::Char('a') if routes => self.check_visible(true),
            KeyCode::Char('n') if routes => self.check_visible(false),
            _ => {}
        }
    }

    /// fzf-like: typing narrows, arrows move, Enter accepts, Esc clears.
    fn on_filter_key(&mut self, key: KeyEvent) {
        if self.navigate(key.code, false) {
            return;
        }
        match key.code {
            KeyCode::Esc => {
                self.filter.clear();
                self.filtering = false;
            }
            KeyCode::Enter => {
                self.filtering = false;
                self.confirm_list();
                return;
            }
            KeyCode::Tab if self.view == View::Routes => {
                self.toggle_route();
                return;
            }
            KeyCode::Backspace => {
                self.filter.pop();
            }
            KeyCode::Char(c) => {
                self.filter.push(c);
                self.list_state_mut().select(Some(0));
            }
            _ => return,
        }
        self.clamp_selection();
    }

    fn on_detail_key(&mut self, key: KeyEvent) {
        let lines = match &self.detail {
            Some(Ok(text)) => text.lines().count(),
            _ => 0,
        };
        let max = u16::try_from(lines.saturating_sub(1)).unwrap_or(u16::MAX);
        let s = self.detail_scroll;
        self.detail_scroll = match key.code {
            KeyCode::Up | KeyCode::Char('k') => s.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => s.saturating_add(1),
            KeyCode::PageUp => s.saturating_sub(15),
            KeyCode::PageDown | KeyCode::Char(' ') => s.saturating_add(15),
            KeyCode::Home | KeyCode::Char('g') => 0,
            KeyCode::End | KeyCode::Char('G') => max,
            KeyCode::Esc | KeyCode::Left | KeyCode::Char('h') => {
                self.view = View::Menu;
                s
            }
            _ => s,
        }
        .min(max);
    }

    // -----------------------------------------------------------------------
    // Routes ticking
    // -----------------------------------------------------------------------

    fn reset_checked(&mut self) {
        self.checked = self
            .route_list()
            .iter()
            .filter(|r| r.selected)
            .map(|r| r.id.clone())
            .collect();
    }

    fn toggle_route(&mut self) {
        let Some(id) = self.current_route().map(|r| r.id.clone()) else {
            return;
        };
        if !self.checked.remove(&id) {
            self.checked.insert(id);
        }
        self.routes_touched = true;
        self.navigate(KeyCode::Down, false);
    }

    fn check_visible(&mut self, on: bool) {
        let ids: Vec<String> = self
            .visible()
            .into_iter()
            .map(|i| self.route_list()[i].id.clone())
            .collect();
        for id in ids {
            if on {
                self.checked.insert(id);
            } else {
                self.checked.remove(&id);
            }
        }
        self.routes_touched = true;
    }

    // -----------------------------------------------------------------------
    // Actions
    // -----------------------------------------------------------------------

    fn confirm_list(&mut self) {
        match self.view {
            View::Profiles => {
                if let Some(name) = self.current_profile().map(|p| p.name.clone()) {
                    self.switch_profile(name);
                }
            }
            View::Routes => self.apply_routes(),
            View::Menu | View::Detail => {}
        }
    }

    fn toggle_connection(&mut self) {
        let up = !self.connected();
        let label = if up { "connecting" } else { "disconnecting" };
        self.start_job(label, move |nb, tx| {
            let ok = run_logged(nb, tx, &[if up { "up" } else { "down" }]);
            if !ok {
                let cmd = if up { "netbird up" } else { "netbird down" };
                return (false, format!("{cmd} failed"));
            }
            let profile = nb.resolve_profile(&nb.status());
            notify_connection(&profile, up);
            signal_waybar();
            let verb = if up { "Connected to" } else { "Disconnected from" };
            (true, format!("{verb} NetBird — profile \"{profile}\""))
        });
    }

    fn switch_profile(&mut self, name: String) {
        if name == self.active_profile() {
            self.flash(format!("already on profile \"{name}\""));
            return;
        }
        let started = self.start_job("switching profile", move |nb, tx| {
            // same sequence as the script: down → select → up (only if it was up)
            let was_connected = nb.status().connected;
            if was_connected {
                run_logged(nb, tx, &["down"]);
            }
            if !run_logged(nb, tx, &["profile", "select", &name]) {
                return (false, format!("could not select profile \"{name}\""));
            }
            notify(&format!("Switched to profile \"{name}\" 🔀"), "dialog-information");
            let mut result = (true, format!("Switched to profile \"{name}\""));
            if was_connected {
                if run_logged(nb, tx, &["up"]) {
                    notify_connection(&name, true);
                } else {
                    result = (false, format!("switched to \"{name}\" but reconnecting failed"));
                }
            }
            signal_waybar();
            result
        });
        if started {
            self.view = View::Menu;
        }
    }

    fn apply_routes(&mut self) {
        let routes = self.route_list();
        if routes.is_empty() {
            self.flash("no routes available");
            return;
        }
        let currently: HashSet<&str> = routes
            .iter()
            .filter(|r| r.selected)
            .map(|r| r.id.as_str())
            .collect();
        let wanted: Vec<String> = routes
            .iter()
            .filter(|r| self.checked.contains(&r.id))
            .map(|r| r.id.clone())
            .collect();
        if wanted.len() == currently.len() && wanted.iter().all(|id| currently.contains(id.as_str())) {
            self.flash("no changes");
            self.view = View::Menu;
            return;
        }
        let all = wanted.len() == routes.len();

        let started = self.start_job("applying routes", move |nb, tx| {
            let (ok, summary) = if wanted.is_empty() {
                (
                    run_logged(nb, tx, &["routes", "deselect", "all"]),
                    "All routes deselected".to_owned(),
                )
            } else if all {
                // `select all` also auto-accepts routes added later
                (
                    run_logged(nb, tx, &["routes", "select", "all"]),
                    "All routes selected".to_owned(),
                )
            } else {
                let mut args = vec!["routes", "select"];
                args.extend(wanted.iter().map(String::as_str));
                (
                    run_logged(nb, tx, &args),
                    format!("Selected routes: {}", wanted.join(", ")),
                )
            };
            if ok {
                notify(&format!("{summary} 🛣️"), "dialog-information");
                (true, summary)
            } else {
                (false, "failed to apply route selection".to_owned())
            }
        });
        if started {
            self.view = View::Menu;
        }
    }
}

/// Run a netbird command, streaming stdout+stderr lines into the log pane
/// (so e.g. an SSO login URL shows up while `netbird up` waits).
fn run_logged(nb: &Netbird, tx: &Sender<Msg>, args: &[&str]) -> bool {
    let _ = tx.send(Msg::Log(format!("$ {}", nb.display(args))));
    let mut child = match nb
        .command(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(err) => {
            let _ = tx.send(Msg::Log(format!("error: {err}")));
            return false;
        }
    };
    let streams: [Option<Box<dyn Read + Send>>; 2] = [
        child.stdout.take().map(|s| Box::new(s) as Box<dyn Read + Send>),
        child.stderr.take().map(|s| Box::new(s) as Box<dyn Read + Send>),
    ];
    let readers: Vec<_> = streams
        .into_iter()
        .flatten()
        .map(|stream| {
            let tx = tx.clone();
            thread::spawn(move || {
                for line in BufReader::new(stream).lines().map_while(Result::ok) {
                    if !line.trim().is_empty() {
                        let _ = tx.send(Msg::Log(line));
                    }
                }
            })
        })
        .collect();
    let ok = child.wait().is_ok_and(|s| s.success());
    for reader in readers {
        let _ = reader.join();
    }
    ok
}

fn matching<T>(items: &[T], keep: impl Fn(&T) -> bool) -> Vec<usize> {
    items
        .iter()
        .enumerate()
        .filter(|(_, item)| keep(item))
        .map(|(i, _)| i)
        .collect()
}

/// Case-insensitive subsequence match (fzf-ish, minus the scoring).
pub fn fuzzy_match(pattern: &str, text: &str) -> bool {
    let mut haystack = text.chars().flat_map(char::to_lowercase);
    pattern
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .all(|p| haystack.any(|c| c == p))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::netbird::{parse_profiles, parse_routes};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn fuzzy() {
        assert!(fuzzy_match("", "anything"));
        assert!(fuzzy_match("wk", "Work"));
        assert!(fuzzy_match("c n", "corp net"));
        assert!(!fuzzy_match("kw", "work"));
    }

    fn connected_app() -> App {
        let mut app = App::new(Netbird::fake(), false);
        let json = serde_json::json!({
            "peers": {"total": 5, "connected": 3}, "cliVersion": "0.83.2", "daemonVersion": "0.83.2",
            "management": {"url": "https://api.netbird.io:443", "connected": true, "error": ""},
            "signal": {"connected": true}, "netbirdIp": "100.92.10.4/16",
            "fqdn": "laptop.netbird.cloud", "profileName": "work"
        });
        app.on_msg(Msg::Status(Status::from_json(&json)));
        app.on_msg(Msg::Profiles(Ok(parse_profiles("Found 3 profiles:\n✗ default\n✓ work\n✗ home\n"))));
        app.on_msg(Msg::Routes(Ok(parse_routes(
            "  - ID: corp net\n    Network: 10.10.0.0/16\n    Status: Selected\n  - ID: dev\n    Domains: dev.example.com\n    Status: Not Selected\n",
        ))));
        app
    }

    fn render(app: &mut App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(100, 22)).unwrap();
        terminal.draw(|frame| crate::ui::draw(frame, app)).unwrap();
        let screen = terminal.backend().to_string();
        println!("{screen}");
        screen
    }

    fn press(app: &mut App, codes: &[KeyCode]) {
        for &code in codes {
            app.on_key(KeyEvent::from(code));
        }
    }

    #[test]
    fn menu_reflects_connection() {
        let mut app = connected_app();
        let screen = render(&mut app);
        assert!(screen.contains("● connected"));
        assert!(screen.contains("disconnect"));
        assert!(screen.contains("profile work"));
        assert!(screen.contains("3/5 peers"));
    }

    #[test]
    fn routes_toggle_marks_pending() {
        let mut app = connected_app();
        press(&mut app, &[KeyCode::Down, KeyCode::Down, KeyCode::Enter]);
        assert_eq!(app.view, View::Routes);
        assert_eq!(app.checked.len(), 1);
        press(&mut app, &[KeyCode::Down, KeyCode::Char(' ')]);
        assert!(app.checked.contains("dev"));
        let screen = render(&mut app);
        assert!(screen.contains("[x] dev"));
        assert!(screen.contains("2/2"));

        // cursor stays on the last row; untick → original selection → nothing to apply
        press(&mut app, &[KeyCode::Char(' '), KeyCode::Enter]);
        assert_eq!(app.view, View::Menu);
        assert!(app.busy.is_none());
        assert_eq!(app.flash.as_ref().map(|(m, _)| m.as_str()), Some("no changes"));
    }

    #[test]
    fn profile_filter() {
        let mut app = connected_app();
        press(&mut app, &[KeyCode::Down, KeyCode::Enter]);
        assert_eq!(app.current_profile().map(|p| p.name.as_str()), Some("work"));
        press(&mut app, &[KeyCode::Char('/'), KeyCode::Char('h'), KeyCode::Char('m')]);
        assert_eq!(app.visible().len(), 1);
        assert_eq!(app.current_profile().map(|p| p.name.as_str()), Some("home"));
        let screen = render(&mut app);
        assert!(screen.contains("profile > hm"));
        press(&mut app, &[KeyCode::Esc]);
        assert_eq!(app.visible().len(), 3);
    }
}
