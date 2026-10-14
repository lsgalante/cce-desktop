//! The desktop's background menu, drawn in this process.
//!
//! A right-click on the bare desktop never reaches this client: its surface
//! takes input over the pinned items alone. The compositor catches it and,
//! while this client subscribes to its `desktop-menu` status topic, hands it
//! over as a `ctl::DesktopMenuRequest` — where (on the output, in layout
//! coordinates, on the desk) and the window that had focus, which the press
//! drops. Without a subscriber it spawns the `cce-desktop-menu` script
//! instead, a `cce-cloud --json` popup with the same rows.
//!
//! The menu is cce-ui's own (`widget::context_menu`), which the runner hosts
//! on a layer surface of its own for a grid app (cce-ui
//! `backend/menu_layer.rs`): so it stands in OUTPUT coordinates, and the
//! request's `x`/`y` are where it opens. Its rows are the host's — no
//! toolkit actions, read back with `take_picked` — and what a row does is a
//! [`Verb`], run off the loop (`run`).

use cce_ui::ipc::ctl::{self, DesktopMenuRequest, MenuWindow, Request};
use cce_ui::process::{de_bin, spawn_detached};
use cce_ui::widget::context_menu::{self, PageTurn, MARK_OFF, MARK_ON};
use cce_ui::widget::{ElementState, MouseButton, WidgetId};

/// The menu's rows run nothing of the toolkit's: there is no target.
const NO_TARGET: WidgetId = WidgetId(0);

/// The modes the Window Mode page offers, as labelled and as `set-mode`
/// spells them.
const MODES: [(&str, &str); 3] = [("Floating", "floating"), ("Tiled", "tiled"), ("Fullscreen", "fullscreen")];

/// What a row does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verb {
    Terminal,
    Files,
    DataEditor,
    Apps,
    Settings,
    Overview,
    /// Set the named window's mode (`set-mode`), and hand it focus back.
    Mode(&'static str),
    Minimize,
    Close,
    Reload,
    Logout,
}

/// The open menu: the request it answers and what the shown page's rows do
/// (`None` for the page row, whose press turns the menu rather than runs).
pub struct DesktopMenu {
    request: DesktopMenuRequest,
    generation: u64,
    rows: Vec<Option<Verb>>,
    on_page: bool,
}

impl DesktopMenu {
    /// Show the menu at the request's point.
    pub fn open(request: DesktopMenuRequest) -> DesktopMenu {
        let mut menu = DesktopMenu { request, generation: 0, rows: Vec::new(), on_page: false };
        menu.show_main(None);
        menu
    }

    /// The main page: at the request's point, or — turned back to from the
    /// mode page — at the corner of the plate it replaces.
    fn show_main(&mut self, turned_from: Option<(f32, f32)>) {
        let mut rows: Vec<(&str, Option<Verb>)> = vec![
            ("Terminal", Some(Verb::Terminal)),
            ("Files", Some(Verb::Files)),
            ("Data Editor", Some(Verb::DataEditor)),
            ("Applications", Some(Verb::Apps)),
            ("System Settings", Some(Verb::Settings)),
            ("Overview", Some(Verb::Overview)),
        ];
        if self.request.window.is_some() {
            rows.push(("Window Mode", None));
            rows.push(("Minimize", Some(Verb::Minimize)));
            rows.push(("Close Window", Some(Verb::Close)));
        }
        rows.push(("Reload Config", Some(Verb::Reload)));
        rows.push(("Logout", Some(Verb::Logout)));
        let labels = rows.iter().map(|(l, _)| l.to_string()).collect();
        match turned_from {
            Some((x, y)) => context_menu::show_page(x, y, None, labels, 0, NO_TARGET),
            None => context_menu::show(self.request.x as f32, self.request.y as f32, labels, 0, NO_TARGET),
        }
        if let Some(page_row) = rows.iter().position(|(_, v)| v.is_none()) {
            context_menu::set_row_page(page_row);
        }
        self.rows = rows.into_iter().map(|(_, v)| v).collect();
        self.generation = context_menu::generation();
        self.on_page = false;
    }

    /// The Window Mode page, its corner where the main plate stood: the
    /// window's current mode marked, a back band named after it.
    fn show_modes(&mut self, at: (f32, f32)) {
        let Some(window) = self.request.window.as_ref() else { return };
        let labels = MODES
            .iter()
            .map(|(label, mode)| format!("{}{label}", if window.mode == *mode { MARK_ON } else { MARK_OFF }))
            .collect();
        context_menu::show_page(at.0, at.1, Some(&window.app_id), labels, 0, NO_TARGET);
        self.rows = MODES.iter().map(|(_, mode)| Some(Verb::Mode(mode))).collect();
        self.generation = context_menu::generation();
        self.on_page = true;
    }

    /// Whether this menu is the one up: the runner closes it on Escape and
    /// on a click elsewhere without telling the app.
    pub fn is_up(&self) -> bool {
        context_menu::is_visible() && context_menu::generation() == self.generation
    }

    /// A press on the menu, in output coordinates: the verb of the row it
    /// ran, if any. A press on the page row or the back band turns the menu
    /// and leaves it up; anywhere else it closes.
    pub fn press(&mut self, button: MouseButton, x: f32, y: f32) -> Option<Verb> {
        context_menu::mouse_input(button, ElementState::Pressed, x, y, None);
        if let Some(turn) = context_menu::take_turn() {
            if !self.turn(turn) {
                context_menu::hide();
            }
            return None;
        }
        self.picked()
    }

    /// Enter on the highlighted row: as a press on it.
    pub fn activate_hovered(&mut self) -> Option<Verb> {
        if let Some(row) = context_menu::hovered_item().filter(|&row| context_menu::leads_to_page(row)) {
            self.turn(PageTurn::Into(row));
            return None;
        }
        context_menu::activate_hovered(None);
        self.picked()
    }

    /// The row a press or Enter ran, read once; the menu is closed.
    fn picked(&mut self) -> Option<Verb> {
        let generation = self.generation;
        let verb = context_menu::take_picked()
            .filter(|&(g, _)| g == generation)
            .and_then(|(_, row)| self.rows.get(row).copied().flatten());
        context_menu::hide();
        verb
    }

    /// Turn into the mode page, or back from it; `false` (and nothing done)
    /// for a turn this menu has not got — Back from the main page, say.
    pub fn turn(&mut self, turn: PageTurn) -> bool {
        let at = (context_menu::x(), context_menu::y());
        match turn {
            PageTurn::Into(_) if !self.on_page => self.show_modes(at),
            PageTurn::Back if self.on_page => self.show_main(Some(at)),
            _ => return false,
        }
        true
    }

    /// Run `verb` for this menu's request, on a thread of its own: each step
    /// is a control-socket round trip, and the reply waits on the
    /// compositor's loop.
    pub fn run(&self, verb: Verb) {
        let (lx, ly) = (self.request.lx, self.request.ly);
        let window = self.request.window.clone();
        std::thread::spawn(move || run(verb, lx, ly, window));
    }
}

/// Send one control request, logging what fails.
fn send(req: Request) {
    let line = req.to_string();
    if let Err(e) = ctl::send(&line, Some(std::time::Duration::from_secs(5))) {
        log::warn!("[menu] {line}: {e}");
    }
}

/// Launch `command` (a shell line) through the compositor, so it runs in the
/// session's environment as a key binding's would — this client is a user
/// service, whose own environment is systemd's — after asking that its first
/// window cover the grid square under the click (`place-next-cell`, keyed by
/// the program's name, which the compositor matches loosely to the app_id).
fn launch_here(app_id: &str, command: &str, lx: f64, ly: f64) {
    send(Request::PlaceNext { app_id: app_id.to_string(), x: lx, y: ly, cell: true });
    send(Request::Spawn { command: command.to_string() });
}

/// The DE's default terminal, as the launcher resolves it: `$TERMINAL`, then
/// config.kdl's `default_terminal` (the settings app's Default Apps pick),
/// then foot. `$TERMINAL` here is this service's; the session's own is what
/// the compositor would hand the spawn, and both come from startcce.
fn terminal() -> String {
    let on_path = |name: &str| {
        !name.is_empty()
            && std::env::var_os("PATH").is_some_and(|path| std::env::split_paths(&path).any(|d| d.join(name).is_file()))
    };
    if let Ok(t) = std::env::var("TERMINAL") {
        if on_path(&t) {
            return t;
        }
    }
    let configured = std::fs::read_to_string(cce_ui::config::get_config_path())
        .ok()
        .and_then(|text| {
            text.lines().find_map(|line| {
                let rest = line.trim().strip_prefix("default_terminal")?.trim();
                let rest = rest.strip_prefix('"')?;
                Some(rest[..rest.find('"')?].to_string())
            })
        });
    match configured {
        Some(t) if on_path(&t) => t,
        _ => "foot".to_string(),
    }
}

fn run(verb: Verb, lx: f64, ly: f64, window: Option<MenuWindow>) {
    let bin = |name: &str| de_bin(name).display().to_string();
    match verb {
        Verb::Terminal => {
            let term = terminal();
            launch_here(&term, &term, lx, ly);
        }
        Verb::Files => launch_here("cce-files", &bin("cce-files"), lx, ly),
        Verb::DataEditor => launch_here("cce-data-editor", &bin("cce-data-editor"), lx, ly),
        Verb::Settings => launch_here("cce-system-interface", &bin("cce-system-interface"), lx, ly),
        Verb::Apps => {
            // The launcher, AT the click: it passes the position on to
            // whatever it launches, so an app picked from it opens on this
            // square too.
            let mut cmd = std::process::Command::new(de_bin("cce-cloud"));
            cmd.args(["--mode", "apps", "-x", &(lx as i32).to_string(), "-y", &(ly as i32).to_string()])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            if let Err(e) = spawn_detached(cmd) {
                log::warn!("[menu] cce-cloud: {e}");
            }
        }
        Verb::Overview => send(Request::Overview),
        Verb::Mode(mode) => {
            // The press dropped the window's focus for the menu's sake; the
            // window just acted on is the one to hand it back to.
            if let Some(w) = window {
                send(Request::SetMode { mode: mode.to_string(), window: Some(w.id.to_string()) });
                send(Request::FocusWindow { query: w.id.to_string(), wait: false });
            }
        }
        Verb::Minimize | Verb::Close => {
            // Both act on the focused window, and focus was dropped.
            if let Some(w) = window {
                send(Request::FocusWindow { query: w.id.to_string(), wait: false });
                send(if verb == Verb::Minimize { Request::Minimize } else { Request::Close });
            }
        }
        Verb::Reload => send(Request::Reload),
        Verb::Logout => send(Request::Exit),
    }
}
