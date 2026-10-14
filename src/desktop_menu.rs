//! The desktop's menus, drawn in this process: the background menu, a
//! window's own menu, and a pinned item's.
//!
//! A right-click on the bare desktop never reaches this client: its surface
//! takes input over the pinned items alone. The compositor catches it and,
//! while this client subscribes to its `desktop-menu` status topic, hands it
//! over as a `ctl::DesktopMenuRequest` — where (on the output, in layout
//! coordinates, on the desk) and the window that had focus, which the press
//! drops. Without a subscriber it spawns the `cce-desktop-menu` script
//! instead, a `cce-cloud --json` popup with the same rows.
//!
//! A window's menu — a right-click on its handle disc, or on the window in
//! overview — comes the same way on the `window-menu` topic, the request
//! naming the window clicked ([`Kind::Window`]); its fallback is the
//! `cce-app-menu` script.
//!
//! An item's menu ([`Kind::Item`]) is this client's own from the start: a
//! right-click on an item lands on this surface. It opens at the pointer
//! the compositor reports (`pointer-location`, asked off the loop), and a
//! row it runs comes back as the item action the app already dispatches
//! (`Verb::Item`, run by the caller rather than [`DesktopMenu::run`]).
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

/// Which menu: the bare desktop's, or a window's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Desktop,
    Window,
    /// A pinned item's: the [`ItemTarget`] it was opened on.
    Item,
}

/// What an item menu is about: the item's canvas node (by id, not index —
/// the list may change while the menu is up), its name for the title, and
/// its rows as `(action, label)`.
#[derive(Clone, Debug)]
pub struct ItemTarget {
    pub node: String,
    pub title: String,
    pub entries: Vec<(&'static str, &'static str)>,
}

/// What a row of the shown page is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Row {
    /// A title: runs nothing.
    Header,
    /// The Window Mode row: turns the menu to its page.
    Page,
    Run(Verb),
}

/// What a row does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verb {
    Terminal,
    Files,
    DataEditor,
    Apps,
    Settings,
    Overview,
    /// Set the named window's mode (`set-mode`) — and, from the background
    /// menu, whose press dropped its focus, hand it focus back.
    Mode(&'static str),
    /// An item menu's row: the action the app runs on the item
    /// (`GridApp::menu_action`).
    Item(&'static str),
    Minimize,
    /// Restart the window's app (`restart-app`): close it, start it again.
    Restart,
    Close,
    Reload,
    Logout,
}

/// The open menu: which, the request it answers, and what the shown page's
/// rows are.
pub struct DesktopMenu {
    kind: Kind,
    request: DesktopMenuRequest,
    item: Option<ItemTarget>,
    generation: u64,
    rows: Vec<Row>,
    on_page: bool,
}

impl DesktopMenu {
    /// Show the menu at the request's point. A window menu without a window
    /// (a malformed line) opens nothing.
    pub fn open(kind: Kind, request: DesktopMenuRequest) -> Option<DesktopMenu> {
        if kind == Kind::Window && request.window.is_none() {
            return None;
        }
        let mut menu = DesktopMenu { kind, request, item: None, generation: 0, rows: Vec::new(), on_page: false };
        menu.show_main(None);
        Some(menu)
    }

    /// Show `item`'s menu at `(x, y)` on the output.
    pub fn open_item(x: f64, y: f64, item: ItemTarget) -> DesktopMenu {
        let request = DesktopMenuRequest { x, y, lx: x, ly: y, vx: 0.0, vy: 0.0, window: None };
        let mut menu =
            DesktopMenu { kind: Kind::Item, request, item: Some(item), generation: 0, rows: Vec::new(), on_page: false };
        menu.show_main(None);
        menu
    }

    /// The canvas node an item menu was opened on.
    pub fn item_node(&self) -> Option<&str> {
        self.item.as_ref().map(|i| i.node.as_str())
    }

    /// The main page: at the request's point, or — turned back to from the
    /// mode page — at the corner of the plate it replaces.
    fn show_main(&mut self, turned_from: Option<(f32, f32)>) {
        let window_rows = [
            ("Window Mode", Row::Page),
            ("Minimize", Row::Run(Verb::Minimize)),
            ("Restart App", Row::Run(Verb::Restart)),
            ("Close Window", Row::Run(Verb::Close)),
        ];
        let mut rows: Vec<(String, Row)> = Vec::new();
        match (self.kind, self.request.window.as_ref()) {
            // A window's menu is titled with the window, as cce-app-menu's
            // popup was, and holds its verbs alone.
            (Kind::Window, Some(w)) => {
                rows.push((w.app_id.clone(), Row::Header));
                rows.extend(window_rows.map(|(l, r)| (l.to_string(), r)));
            }
            (Kind::Window, None) => {}
            // An item's menu is titled with the item, as cce-cloud's popup
            // was.
            (Kind::Item, _) => {
                if let Some(item) = self.item.as_ref() {
                    rows.push((item.title.clone(), Row::Header));
                    rows.extend(item.entries.iter().map(|(id, label)| (label.to_string(), Row::Run(Verb::Item(*id)))));
                }
            }
            (Kind::Desktop, window) => {
                rows.extend(
                    [
                        ("Terminal", Verb::Terminal),
                        ("Files", Verb::Files),
                        ("Data Editor", Verb::DataEditor),
                        ("Applications", Verb::Apps),
                        ("System Settings", Verb::Settings),
                        ("Overview", Verb::Overview),
                    ]
                    .map(|(l, v)| (l.to_string(), Row::Run(v))),
                );
                if window.is_some() {
                    rows.extend(window_rows.map(|(l, r)| (l.to_string(), r)));
                }
                rows.push(("Reload Config".to_string(), Row::Run(Verb::Reload)));
                rows.push(("Logout".to_string(), Row::Run(Verb::Logout)));
            }
        }
        let headers = rows.iter().take_while(|(_, r)| *r == Row::Header).count();
        let labels = rows.iter().map(|(l, _)| l.clone()).collect();
        match turned_from {
            Some((x, y)) => context_menu::show_page(x, y, None, labels, headers, NO_TARGET),
            None => context_menu::show(self.request.x as f32, self.request.y as f32, labels, headers, NO_TARGET),
        }
        if let Some(page_row) = rows.iter().position(|(_, r)| *r == Row::Page) {
            context_menu::set_row_page(page_row);
        }
        self.rows = rows.into_iter().map(|(_, r)| r).collect();
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
        self.rows = MODES.iter().map(|(_, mode)| Row::Run(Verb::Mode(mode))).collect();
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
            .and_then(|(_, row)| match self.rows.get(row) {
                Some(Row::Run(verb)) => Some(*verb),
                _ => None,
            });
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
        let (kind, lx, ly) = (self.kind, self.request.lx, self.request.ly);
        let window = self.request.window.clone();
        std::thread::spawn(move || run(kind, verb, lx, ly, window));
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

fn run(kind: Kind, verb: Verb, lx: f64, ly: f64, window: Option<MenuWindow>) {
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
            if let Some(w) = window {
                send(Request::SetMode { mode: mode.to_string(), window: Some(w.id.to_string()) });
                // A background press dropped the window's focus for the
                // menu's sake: the window just acted on is the one to hand it
                // back to. A press on a window's own handle dropped nothing.
                if kind == Kind::Desktop {
                    send(Request::FocusWindow { query: w.id.to_string(), wait: false });
                }
            }
        }
        Verb::Minimize | Verb::Close => {
            // Both act on the focused window, and focus was dropped.
            if let Some(w) = window {
                send(Request::FocusWindow { query: w.id.to_string(), wait: false });
                send(if verb == Verb::Minimize { Request::Minimize } else { Request::Close });
            }
        }
        Verb::Restart => {
            // By id, not focus: the compositor closes every window of that
            // window's process, waits for it to exit and starts it again.
            // The reply waits for all of that — up to 15 s when the app
            // will not go (a save prompt), and then it says so.
            if let Some(w) = window {
                let line = format!("restart-app {}", w.id);
                match ctl::send(&line, Some(std::time::Duration::from_secs(20))) {
                    Ok(reply) if reply.starts_with("error") => log::warn!("[menu] {line}: {}", reply.trim_end()),
                    Ok(reply) => log::info!("[menu] {line}: {}", reply.trim_end()),
                    Err(e) => log::warn!("[menu] {line}: {e}"),
                }
            }
        }
        // The caller runs an item's action itself (`DesktopMenu` docs).
        Verb::Item(_) => {}
        Verb::Reload => send(Request::Reload),
        Verb::Logout => send(Request::Exit),
    }
}
