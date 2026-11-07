//! App launcher: lists XDG .desktop apps, type to filter, Enter launches, Escape quits.
//! It does not close on focus loss: with focus-follows-mouse, moving the pointer off would close it.
//! The gear button or Ctrl+, opens layout settings, saved to $XDG_CONFIG_HOME/sh.bzn.launcher/settings.

use std::{collections::HashSet, fs, os::unix::process::CommandExt, path::{Path, PathBuf}, process::Command};

use rune::tok::{self, type_};
use rune::{App, AppSpec, ScrollSpec, Color, Cx, El, Image, ImageCache, Key, KeyEvent, TextEdit, Tx, WindowOptions, WindowSpec, col, el, row, spacer, text, text_input};

/// The Wayland app_id / X11 class and the config folder name.
const APP_ID: &str = "sh.bzn.launcher";
/// Padding around the results, and the gap between rows and between grid cells.
const LIST_PAD: f32 = 6.0;
const GAP: f32 = 2.0;
/// The height of the line between the pins and the other apps, with its margins.
const DIVIDER_H: f32 = 8.5;
/// The size of the search and settings icons in the header.
const HEADER_ICON: f32 = 18.0;

/// One app. `id` is the .desktop file name, the key for pins and launch counts.
struct Entry { name: String, id: String, exec: Vec<String>, terminal: bool, path: PathBuf }

/// An XDG base directory: the variable if it holds an absolute path, else `fallback` under $HOME.
/// The spec says an empty or relative value counts as unset.
fn xdg_dir(var: &str, fallback: &str) -> PathBuf {
    std::env::var(var).ok().map(PathBuf::from).filter(|p| p.is_absolute())
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(fallback))
}

/// $XDG_DATA_DIRS without its empty and relative entries, or the spec default when none are left.
fn data_dirs(var: Option<String>) -> Vec<PathBuf> {
    let dirs: Vec<PathBuf> = var.unwrap_or_default().split(':').map(PathBuf::from).filter(|p| p.is_absolute()).collect();
    if dirs.is_empty() { vec!["/usr/local/share".into(), "/usr/share".into()] } else { dirs }
}

/// Write a file, making its folder first. A lost setting or launch count is not worth a crash.
fn write_file(path: PathBuf, text: String) {
    if let Some(dir) = path.parent() { let _ = fs::create_dir_all(dir); }
    let _ = fs::write(path, text);
}

/// Corner styles, as in hokum's notification cards.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Corners { Sharp, Rounded, Soft, Round }

impl Corners {
    const ALL: [(Corners, &'static str); 4] =
        [(Corners::Sharp, "sharp"), (Corners::Rounded, "rounded"), (Corners::Soft, "soft"), (Corners::Round, "round")];
    /// (window radius, radius of the items inside it)
    fn radii(self) -> (f32, f32) {
        match self {
            Corners::Sharp => (4.0, 4.0),
            Corners::Rounded => (10.0, 6.0),
            Corners::Soft => (16.0, 7.0),
            Corners::Round => (24.0, 10.0),
        }
    }
    fn name(self) -> &'static str { Self::ALL.iter().find(|c| c.0 == self).unwrap().1 }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Settings { grid: bool, icon: f32, names: bool, tall: bool, corners: Corners, shadow: u8 }

impl Default for Settings {
    fn default() -> Self { Settings { grid: false, icon: 24.0, names: true, tall: false, corners: Corners::Rounded, shadow: 0 } }
}

impl Settings {
    fn path() -> PathBuf { xdg_dir("XDG_CONFIG_HOME", ".config").join(APP_ID).join("settings") }
    fn parse(src: &str) -> Self {
        let mut s = Settings::default();
        for (k, v) in src.lines().filter_map(|l| l.split_once('=')) {
            match k.trim() {
                "grid" => s.grid = v.trim() == "true",
                "icon" => s.icon = v.trim().parse().unwrap_or(s.icon),
                "names" => s.names = v.trim() == "true",
                "tall" => s.tall = v.trim() == "true",
                "corners" => if let Some(c) = Corners::ALL.iter().find(|c| c.1 == v.trim()) { s.corners = c.0 },
                "shadow" => s.shadow = v.trim().parse::<u8>().unwrap_or(0).min(2),
                _ => {}
            }
        }
        s
    }
    fn load() -> Self { fs::read_to_string(Self::path()).map(|s| Self::parse(&s)).unwrap_or_default() }
    fn save(&self) { write_file(Self::path(), self.to_text()) }
    fn to_text(&self) -> String {
        format!("grid={}\nicon={}\nnames={}\ntall={}\ncorners={}\nshadow={}\n", self.grid, self.icon, self.names, self.tall, self.corners.name(), self.shadow)
    }
    /// The card size.
    fn card(&self) -> (f32, f32) { (560.0, if self.tall { 620.0 } else { 380.0 }) }
    /// The window never resizes: Hyprland ignores a client's resize request. It opens big enough
    /// for the tallest card with the strongest shadow, and the card sits centred inside it.
    fn window() -> (f32, f32) {
        let max = Settings { tall: true, shadow: 2, ..Settings::default() };
        let (w, h) = max.card();
        (w + 2.0 * max.margin(), h + 2.0 * max.margin())
    }
    /// Shadow level 0 (none), 1 (soft), 2 (strong) as (y offset, blur, opacity).
    fn shadow_spec(&self) -> Option<(f32, f32, f32)> {
        match self.shadow { 1 => Some((6.0, 24.0, 0.35)), 2 => Some((10.0, 40.0, 0.6)), _ => None }
    }
    /// Room around the card so the shadow is not cut off at the window edge.
    fn margin(&self) -> f32 { self.shadow_spec().map_or(0.0, |(y, blur, _)| y + blur) }
    /// Grid cell width, and how many cells (with the gaps between them) fit in a row.
    fn cell(&self) -> f32 { self.icon + 64.0 }
    /// The fixed height of one result row: a list row, or a grid cell with its icon and name.
    fn row_h(&self) -> f32 {
        if !self.grid { return self.icon + 12.0 }
        self.icon + 20.0 + if self.names { 23.0 } else { 0.0 }
    }
    fn cols(&self) -> usize {
        if !self.grid { return 1 }
        (((self.card().0 - 2.0 * LIST_PAD + GAP) / (self.cell() + GAP)) as usize).max(1)
    }
}

/// Launch counts by .desktop file name, in $XDG_STATE_HOME/sh.bzn.launcher/usage.
type Usage = std::collections::HashMap<String, u32>;

fn usage_path() -> PathBuf { xdg_dir("XDG_STATE_HOME", ".local/state").join(APP_ID).join("usage") }

fn load_usage() -> Usage {
    fs::read_to_string(usage_path()).unwrap_or_default().lines()
        .filter_map(|l| { let (n, id) = l.split_once('\t')?; Some((id.to_string(), n.parse().ok()?)) })
        .collect()
}

fn save_usage(u: &Usage) { write_file(usage_path(), u.iter().map(|(id, n)| format!("{n}\t{id}\n")).collect()) }

/// Pinned apps, one .desktop file name per line, in $XDG_CONFIG_HOME/sh.bzn.launcher/pinned.
fn pinned_path() -> PathBuf { Settings::path().with_file_name("pinned") }

/// The pins, without blank lines and repeats (a hand-edited file can have both).
fn parse_pinned(src: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    src.lines().map(str::trim).filter(|l| !l.is_empty() && seen.insert(*l)).map(String::from).collect()
}

/// Pin the picked app, or unpin it. The list reorders, so the selection stays on the same app,
/// or goes to the top if the app left the list. False when no app is picked.
fn flip_pin(s: &mut State) -> bool {
    let Some(&i) = matches(s).get(s.picked) else { return false };
    let id = s.apps[i].id.clone();
    match s.pinned.iter().position(|p| *p == id) {
        Some(at) => { s.pinned.remove(at); }
        None => s.pinned.push(id),
    }
    s.picked = matches(s).iter().position(|&j| j == i).unwrap_or(0);
    true
}

/// Ctrl+P: flip the pin, save the pins, and bring the app into view in its new place.
fn toggle_pin(s: &mut State, tx: &mut Tx) {
    if !flip_pin(s) { return }
    write_file(pinned_path(), s.pinned.iter().map(|id| format!("{id}\n")).collect());
    show_picked(s, &matches(s), tx);
}

struct State {
    query: TextEdit,
    usage: Usage,
    pinned: Vec<String>,
    picked: usize,
    apps: Vec<Entry>,
    icons: Vec<Image>,
    _cache: Option<ImageCache>,
    settings: Settings,
    show_settings: bool,
    /// Why the last launch failed. Typing clears it.
    error: Option<String>,
}

fn parse(src: &str, path: PathBuf) -> Option<Entry> {
    let (mut name, mut exec, mut terminal, mut in_main) = (None, None, false, false);
    for line in src.lines() {
        if line.starts_with('[') { in_main = line == "[Desktop Entry]"; continue }
        if !in_main { continue }
        // The spec ignores spaces around the `=`; trailing spaces would break `Terminal=true `.
        let Some((k, v)) = line.split_once('=') else { continue };
        match (k.trim_end(), v.trim()) {
            ("Name", v) => name = Some(v.to_string()),
            ("Exec", v) => exec = Some(v.to_string()),
            ("Terminal", v) => terminal = v == "true",
            ("NoDisplay" | "Hidden", "true") => return None,
            ("Type", v) if v != "Application" => return None,
            _ => {}
        }
    }
    let name = name?;
    let exec = exec_args(&exec?, &name, &path);
    if exec.is_empty() { return None }
    let id = path.file_name()?.to_string_lossy().into_owned();
    Some(Entry { name, id, exec, terminal, path })
}

/// Split an Exec value into arguments, per the Desktop Entry spec. It is not a shell line: only
/// double quotes group, and in them a backslash escapes the next character. The launcher passes
/// no files, so %f %F %u %U and the deprecated codes expand to nothing. %c is the name, %k the
/// .desktop file, %% a literal %.
/// ponytail: %i (--icon <Icon>) expands to nothing; add it when an app needs it.
fn exec_args(v: &str, name: &str, path: &Path) -> Vec<String> {
    // First the escapes of every string value. Others stay for the quote rules below.
    let mut src = String::new();
    let mut it = v.chars();
    while let Some(c) = it.next() {
        if c != '\\' { src.push(c); continue }
        match it.next() {
            Some('s') => src.push(' '),
            Some('n') => src.push('\n'),
            Some('t') => src.push('\t'),
            Some('r') => src.push('\r'),
            Some('\\') => src.push('\\'),
            Some(o) => { src.push('\\'); src.push(o) }
            None => src.push('\\'),
        }
    }
    // `open`: an argument has started, so an empty "" still counts and a lone %U does not.
    let (mut args, mut cur, mut open, mut quoted) = (vec![], String::new(), false, false);
    let mut it = src.chars();
    while let Some(c) = it.next() {
        match c {
            '"' => { quoted = !quoted; open = true }
            '\\' if quoted => { if let Some(n) = it.next() { cur.push(n) } }
            ' ' | '\t' | '\n' if !quoted => { if open { args.push(std::mem::take(&mut cur)); open = false } }
            '%' => match it.next() {
                Some('%') => { cur.push('%'); open = true }
                Some('c') => { cur.push_str(name); open = true }
                Some('k') => { cur.push_str(&path.to_string_lossy()); open = true }
                _ => {}
            },
            _ => { cur.push(c); open = true }
        }
    }
    if open { args.push(cur) }
    args
}

fn load_apps() -> Vec<Entry> {
    let dirs = std::iter::once(xdg_dir("XDG_DATA_HOME", ".local/share")).chain(data_dirs(std::env::var("XDG_DATA_DIRS").ok()));
    let mut seen = HashSet::new();
    let mut apps: Vec<Entry> = dirs
        .flat_map(|d| fs::read_dir(d.join("applications")).into_iter().flatten().flatten())
        .filter(|f| f.path().extension().is_some_and(|e| e == "desktop"))
        .filter(|f| seen.insert(f.file_name())) // first dir wins, per XDG
        .filter_map(|f| parse(&fs::read_to_string(f.path()).ok()?, f.path()))
        .collect();
    apps.sort_by_key(|a| a.name.to_lowercase());
    apps
}

fn matches(s: &State) -> Vec<usize> {
    let q = s.query.text.to_lowercase();
    if q.is_empty() {
        // Pinned apps in pin order, then the most launched; ties (and apps never launched)
        // keep alphabetical order. The list goes on under the pins with every other app.
        let pinned = s.pinned.iter().filter_map(|id| s.apps.iter().position(|a| a.id == *id));
        let mut rest: Vec<usize> = (0..s.apps.len()).filter(|&i| !s.pinned.contains(&s.apps[i].id)).collect();
        rest.sort_by_key(|&i| std::cmp::Reverse(s.usage.get(&s.apps[i].id).copied().unwrap_or(0)));
        return pinned.chain(rest).collect();
    }
    (0..s.apps.len()).filter(|&i| s.apps[i].name.to_lowercase().contains(&q)).collect()
}

/// How many of `found` are pins. Only an empty query puts the pins first.
fn pin_count(s: &State, found: &[usize]) -> usize {
    if !s.query.text.is_empty() { return 0 }
    found.iter().take_while(|&&i| s.pinned.contains(&s.apps[i].id)).count()
}

/// The result rows as (first index, length). The pins fill their own rows, so the rest starts
/// on a new row.
fn rows(pins: usize, total: usize, cols: usize) -> Vec<(usize, usize)> {
    let run = |from: usize, to: usize| (from..to).step_by(cols).map(move |r| (r, cols.min(to - r)));
    run(0, pins).chain(run(pins, total)).collect()
}

/// Up/Down: move one row, wrapping at the ends. Keep the column, or take the last cell of a short row.
fn row_step(rows: &[(usize, usize)], picked: usize, d: isize) -> usize {
    let Some(r) = rows.iter().position(|&(start, len)| picked < start + len) else { return 0 };
    let column = picked - rows[r].0;
    let (start, len) = rows[(r as isize + d).rem_euclid(rows.len() as isize) as usize];
    start + column.min(len - 1)
}

/// The terminal for Terminal=true apps: $TERMINAL, else xdg-terminal-exec. Both must take `-e command`.
fn terminal() -> Vec<String> {
    let t = std::env::var("TERMINAL").ok().filter(|t| !t.trim().is_empty()).unwrap_or_else(|| "xdg-terminal-exec".into());
    t.split_whitespace().map(String::from).chain(["-e".to_string()]).collect()
}

/// The command for an app: its Exec, inside `term` when it is a Terminal=true app.
fn argv(app: &Entry, term: Vec<String>) -> Vec<String> {
    if app.terminal { term.into_iter().chain(app.exec.iter().cloned()).collect() } else { app.exec.clone() }
}

/// Start app `i`. If it starts, count the launch. If not, keep the reason in `s.error`.
fn start(s: &mut State, i: usize) -> bool {
    let argv = argv(&s.apps[i], terminal());
    // Its own process group, so the app outlives the launcher and gets none of its signals.
    if let Err(e) = Command::new(&argv[0]).args(&argv[1..]).process_group(0).spawn() {
        s.error = Some(format!("Could not start {}: {e}", argv[0]));
        return false;
    }
    *s.usage.entry(s.apps[i].id.clone()).or_default() += 1;
    save_usage(&s.usage);
    true
}

/// Enter or a click: start the picked app and close. A failed start keeps the window open.
fn launch(s: &mut State, tx: &mut Tx) {
    // The settings panel hides the list, so Enter there must not start an app.
    if s.show_settings { return }
    let Some(&i) = matches(s).get(s.picked) else { return };
    if start(s, i) { tx.close_window() }
}

/// Move the selection and scroll it into view. Up/Down move by row, Left/Right by item; both wrap.
fn step(s: &mut State, d: isize, by_row: bool, tx: &mut Tx) {
    let found = matches(s);
    if found.is_empty() { return }
    s.picked = if by_row {
        row_step(&rows(pin_count(s, &found), found.len(), s.settings.cols()), s.picked, d)
    } else {
        (s.picked as isize + d).rem_euclid(found.len() as isize) as usize
    };
    show_picked(s, &found, tx);
}

/// Scroll the picked app into view, and let the highlight slide to it.
fn show_picked(s: &State, found: &[usize], tx: &mut Tx) {
    if let Some(i) = found.get(s.picked) { tx.scroll_into_view(format!("app-{i}")) }
    // Keys in the search field can count as typing, which lands at once; the highlight must slide.
    tx.animated();
}

/// A click on a button takes focus; give it back to the search field so the keys keep working.
fn toggle_settings(s: &mut State) { s.show_settings = !s.show_settings; s.query.focused = true }

/// The app's icon, or a gradient circle with its initial when it has none.
fn icon(s: &State, i: usize) -> El {
    let size = s.settings.icon;
    // An icon can be missing before setup fills the list (and in tests).
    if let Some(img) = s.icons.get(i).filter(|img| img.error().is_none()) { return el().size(size, size).pixels(img.pixels()) }
    let initial = s.apps[i].name.chars().next().unwrap_or('?').to_uppercase().to_string();
    // The name picks the hue, so each app keeps its colour between runs.
    let h = s.apps[i].name.bytes().map(usize::from).sum::<usize>();
    let font = if size >= 32.0 { type_::TITLE } else { type_::LABEL };
    el().size(size, size).radius(size / 2.0).center()
        .gradient(tok::accent(h, true), tok::accent(h + 1, true))
        .child(text(initial).font(font).color(Color::WHITE))
}

fn results(s: &State) -> El {
    let t = tok::theme(true);
    let set = s.settings;
    let found = matches(s);
    let mut list = col().key("results").grow(1.0).min_h(0.0).padding(LIST_PAD).gap(GAP).scroll(ScrollSpec { stick_bottom: false });
    // Before you type, the pins lead the list; a faint line divides them from the rest, and in
    // the grid the rest starts a new row.
    let pins = pin_count(s, &found);
    let divider = pins.div_ceil(set.cols());
    let rows = rows(pins, found.len(), set.cols());
    // One white highlight under the picked app. It is one element at a calculated place, so it
    // slides on its spring when the selection moves. It is the first child, so the apps draw on it.
    // ponytail: the place comes from the fixed row heights; a row that sizes to its content breaks it.
    if let Some(r) = rows.iter().position(|&(start, len)| s.picked < start + len) {
        let y = LIST_PAD + r as f32 * (set.row_h() + GAP) + if divider > 0 && r >= divider { DIVIDER_H + GAP } else { 0.0 };
        let hl = el().key("highlight").h(set.row_h()).radius(set.corners.radii().1).fill(Color::WHITE)
            .token_motion(tok::motion::SNAP).a11y_hidden();
        list = list.child(if set.grid {
            hl.w(set.cell()).absolute(LIST_PAD + (s.picked - rows[r].0) as f32 * (set.cell() + GAP), y)
        } else {
            hl.pin(Some(LIST_PAD), Some(y), Some(LIST_PAD), None)
        });
    }
    for (n, (start, len)) in rows.into_iter().enumerate() {
        if n == divider && divider > 0 {
            list = list.child(el().h(0.5).mx(64.0).my((DIVIDER_H - 0.5) / 2.0).fill(t.separator.alpha(0.07)));
        }
        let mut line = row().gap(GAP);
        for (c, &i) in found[start..start + len].iter().enumerate() {
            let on = start + c == s.picked;
            let ink = if on { t.bg } else { t.text };
            // The highlighted name is bold: BODY's size at a heavier weight.
            let font = if on { tok::TextStyle::new(type_::BODY.size, 600.0) } else { type_::BODY };
            let name = text(s.apps[i].name.clone()).font(font).color(ink).single_line().truncate();
            let item = if set.grid {
                col().w(set.cell()).h(set.row_h()).px(4.0).gap(6.0).center()
            } else {
                row().w_full().h(set.row_h()).px(12.0).gap(10.0).center_y()
            };
            let pos = start + c;
            let mut item = item.key(format!("app-{i}")).role("button").label(s.apps[i].name.clone())
                .on_press_tx(move |s: &mut State, tx: &mut Tx| { s.picked = pos; s.query.focused = true; launch(s, tx) })
                .radius(set.corners.radii().1)
                .child(icon(s, i));
            if set.names { item = item.child(name) }
            // A pin mark: after the name in the list, in the corner of the cell in the grid.
            if s.pinned.contains(&s.apps[i].id) {
                let pin = pin_icon(if on { t.bg.alpha(0.45) } else { t.text_secondary.alpha(0.6) });
                item = if set.grid { item.child(pin.absolute(set.cell() - 18.0, 6.0)) } else { item.child(spacer()).child(pin) };
            }
            line = line.child(item);
        }
        list = list.child(line);
    }
    // Room under the last app, so it does not touch the bottom edge of the card.
    list.child(el().h(12.0))
}

/// One settings row: a label and a set of buttons, one of them selected.
fn choice(label: &str, options: Vec<(&'static str, bool, fn(&mut Settings))>) -> El {
    let t = tok::theme(true);
    let mut buttons = row().gap(4.0).padding(3.0).radius(8.0).fill(t.control_off);
    for (name, on, apply) in options {
        buttons = buttons.child(el().key(format!("set-{name}")).role("button").label(name).h(28.0).px(12.0).radius(6.0).center()
            .fill(if on { Color::WHITE } else { Color::TRANSPARENT })
            .child(text(name).font(type_::BODY).color(if on { t.bg } else { t.text }))
            .on_press(move |s: &mut State| {
                apply(&mut s.settings);
                s.settings.save();
                s.query.focused = true;
            }));
    }
    row().w_full().h(44.0).px(18.0).center_y()
        .child(text(label.to_string()).font(type_::BODY).color(t.text)).child(spacer()).child(buttons)
}

fn settings_view(s: &State) -> El {
    let set = s.settings;
    col().key("settings").grow(1.0).py(8.0)
        .child(choice("Layout", vec![
            ("List", !set.grid, |s| s.grid = false),
            ("Grid", set.grid, |s| s.grid = true),
        ]))
        .child(choice("Icon size", vec![
            ("Small", set.icon == 24.0, |s| s.icon = 24.0),
            ("Medium", set.icon == 32.0, |s| s.icon = 32.0),
            ("Large", set.icon == 48.0, |s| s.icon = 48.0),
        ]))
        .child(choice("App names", vec![
            ("Show", set.names, |s| s.names = true),
            ("Hide", !set.names, |s| s.names = false),
        ]))
        .child(choice("Window", vec![
            ("Compact", !set.tall, |s| s.tall = false),
            ("Tall", set.tall, |s| s.tall = true),
        ]))
        .child(choice("Corners", vec![
            ("Sharp", set.corners == Corners::Sharp, |s| s.corners = Corners::Sharp),
            ("Rounded", set.corners == Corners::Rounded, |s| s.corners = Corners::Rounded),
            ("Soft", set.corners == Corners::Soft, |s| s.corners = Corners::Soft),
            ("Round", set.corners == Corners::Round, |s| s.corners = Corners::Round),
        ]))
        .child(choice("Shadow", vec![
            ("None", set.shadow == 0, |s| s.shadow = 0),
            ("Soft", set.shadow == 1, |s| s.shadow = 1),
            ("Strong", set.shadow == 2, |s| s.shadow = 2),
        ]))
}

// Lucide icons (ISC licence, lucide.dev), drawn as outlines in the element's colour. Fonts and
// colour emoji cannot do this: a glyph's size and an emoji's colour are fixed.
static PIN: rune::Icon = rune::Icon {
    name: "pin",
    view: 24.0,
    body: r#"<line x1="12" x2="12" y1="17" y2="22" stroke="currentColor" stroke-width="2" stroke-linecap="round"/><path d="M5 17h14v-1.76a2 2 0 0 0-1.11-1.79l-1.78-.9A2 2 0 0 1 15 10.76V6h1a2 2 0 0 0 0-4H8a2 2 0 0 0 0 4h1v4.76a2 2 0 0 1-1.11 1.79l-1.78.9A2 2 0 0 0 5 15.24Z" fill="none" stroke="currentColor" stroke-width="2" stroke-linejoin="round"/>"#,
};

static SEARCH: rune::Icon = rune::Icon {
    name: "search",
    view: 24.0,
    body: r#"<circle cx="11" cy="11" r="8" fill="none" stroke="currentColor" stroke-width="2"/><path d="m21 21-4.3-4.3" stroke="currentColor" stroke-width="2" stroke-linecap="round"/>"#,
};
static SETTINGS: rune::Icon = rune::Icon {
    name: "settings",
    view: 24.0,
    body: r#"<path d="M12.22 2h-.44a2 2 0 0 0-2 2v.18a2 2 0 0 1-1 1.73l-.43.25a2 2 0 0 1-2 0l-.15-.08a2 2 0 0 0-2.73.73l-.22.38a2 2 0 0 0 .73 2.73l.15.1a2 2 0 0 1 1 1.72v.51a2 2 0 0 1-1 1.74l-.15.09a2 2 0 0 0-.73 2.73l.22.38a2 2 0 0 0 2.73.73l.15-.08a2 2 0 0 1 2 0l.43.25a2 2 0 0 1 1 1.73V20a2 2 0 0 0 2 2h.44a2 2 0 0 0 2-2v-.18a2 2 0 0 1 1-1.73l.43-.25a2 2 0 0 1 2 0l.15.08a2 2 0 0 0 2.73-.73l.22-.39a2 2 0 0 0-.73-2.73l-.15-.08a2 2 0 0 1-1-1.74v-.5a2 2 0 0 1 1-1.74l.15-.09a2 2 0 0 0 .73-2.73l-.22-.38a2 2 0 0 0-2.73-.73l-.15.08a2 2 0 0 1-2 0l-.43-.25a2 2 0 0 1-1-1.73V4a2 2 0 0 0-2-2z" fill="none" stroke="currentColor" stroke-width="2" stroke-linejoin="round"/><circle cx="12" cy="12" r="3" fill="none" stroke="currentColor" stroke-width="2"/>"#,
};

fn pin_icon(c: Color) -> El { rune::icon(&PIN, 14.0).color(c) }

/// A keyboard key, for a shortcut hint.
fn key_cap(name: &str) -> El {
    let t = tok::theme(true);
    el().h(20.0).px(6.0).radius(5.0).fill(t.control_track).border(1.0, t.control_off).center()
        .child(text(name.to_string()).font(type_::LABEL).color(t.text_secondary))
}

fn view(s: &State, _: &mut Cx<State>) -> El {
    let t = tok::theme(true);
    let field = text_input(&s.query, type_::BODY.lh(26.0)).key("query").single_line().grow(1.0)
        .color(t.text).placeholder("search apps", t.text_secondary)
        .on_edit(|s: &mut State| &mut s.query)
        .on_input_key(|s: &mut State, ev: &KeyEvent, tx: &mut Tx| {
            match ev.key {
                _ if ev.is_command(',') => toggle_settings(s),
                _ if ev.is_command('p') && !s.show_settings => toggle_pin(s, tx),
                Key::Escape if s.show_settings => s.show_settings = false,
                Key::Escape => tx.close_window(),
                _ if s.show_settings => return false,
                Key::Down => step(s, 1, true, tx),
                Key::Up => step(s, -1, true, tx),
                // Left and Right move the text caret in the list; in the grid they move the selection.
                Key::Right if s.settings.grid => step(s, 1, false, tx),
                Key::Left if s.settings.grid => step(s, -1, false, tx),
                _ => return false,
            }
            true
        })
        .on_input_change(|s: &mut State, _| { s.picked = 0; s.error = None })
        .on_submit(launch);
    let gear = el().key("gear").role("button").label("Settings").size(32.0, 32.0).radius(8.0).center()
        .fill(if s.show_settings { t.control_off } else { Color::TRANSPARENT })
        .child(rune::icon(&SETTINGS, HEADER_ICON).color(t.text_secondary))
        .on_press(toggle_settings);
    let set = s.settings;
    let r = set.corners.radii().0;
    let mut header = row().h(56.0).pl(18.0).pr(12.0).gap(12.0).center_y()
        .child(rune::icon(&SEARCH, HEADER_ICON).color(t.text_secondary)).child(field);
    // The pin shortcut, while the placeholder shows. Ctrl+P does nothing in the settings panel.
    if s.query.text.is_empty() && !s.show_settings {
        header = header.child(row().gap(4.0).center_y().child(key_cap("ctrl")).child(key_cap("p"))
            .child(text("pin").font(type_::BODY).color(t.text_secondary).pl(2.0)));
    }
    // The window is transparent; the rounded card is the visible launcher.
    let mut card = col().w_full().h_full().fill(t.card).radius(r).clip_rounded(r)
        .child(header.child(gear))
        .child(el().h(1.0).w_full().fill(t.separator));
    if let Some(e) = &s.error {
        card = card.child(row().w_full().px(18.0).py(8.0)
            .child(text(e.clone()).font(type_::BODY).color(rune::oklch(0.72, 0.16, 25.0)).single_line().truncate()));
    }
    let card = card.child(if s.show_settings { settings_view(s) } else { results(s) });
    // The shadow sits on a frame around the card, so the card's clip does not cut it off.
    let (w, h) = set.card();
    let mut frame = el().size(w, h).radius(r).child(card);
    if let Some((y, blur, a)) = set.shadow_spec() { frame = frame.shadow(y, blur, Color::BLACK.alpha(a)) }
    col().w_full().h_full().center().child(frame)
}

fn main() {
    // The Wayland app_id / X11 class, so compositor window rules can match it.
    rune::set_app_id(APP_ID);
    rune::run_windows(|_| {
        let app = App::new(AppSpec {
            // Icons decode on the cache's worker; each finished one wakes the app to repaint.
            // They rasterize at the largest size, so a size change needs no new decode.
            setup: Some(|s: &mut State, waker| {
                let cache = ImageCache::new(waker);
                s.icons = s.apps.iter().map(|a| cache.app_icon(&a.path, 48.0)).collect();
                s._cache = Some(cache);
            }),
            ..AppSpec::new("Launcher", Settings::window(), || {
                let mut query = TextEdit::new("");
                query.focused = true;
                let pinned = parse_pinned(&fs::read_to_string(pinned_path()).unwrap_or_default());
                State { query, usage: load_usage(), pinned, picked: 0, apps: load_apps(), icons: vec![], _cache: None, settings: Settings::load(), show_settings: false, error: None }
            }, view)
        });
        let opts = WindowOptions { borderless: true, transparent: true, always_on_top: true, resizable: false, visible: true, ..Default::default() };
        vec![WindowSpec::from_app("launcher", app).configured_options(opts.macos(Default::default()))]
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_query_shows_pins_then_the_most_launched() {
        let app = |n: &str| Entry { name: n.into(), id: format!("{n}.desktop"), exec: vec![], terminal: false, path: format!("{n}.desktop").into() };
        let usage: Usage = [("d.desktop".to_string(), 5), ("b.desktop".to_string(), 2)].into();
        let mut s = State {
            query: TextEdit::new(""), usage, pinned: vec![], picked: 0, apps: ["a", "b", "c", "d", "e"].map(app).into(),
            icons: vec![], _cache: None, settings: Settings::default(), show_settings: false, error: None,
        };
        assert_eq!(matches(&s), [3, 1, 0, 2, 4], "d, b, then the rest alphabetically");
        s.pinned = vec!["e.desktop".into(), "missing.desktop".into()];
        assert_eq!(matches(&s), [4, 3, 1, 0, 2], "the pin first; an uninstalled pin is skipped");
        s.pinned = ["e", "c", "a", "b"].map(|n| format!("{n}.desktop")).into();
        assert_eq!(matches(&s), [4, 2, 0, 1, 3], "pins in pin order, then the rest");
        assert_eq!(pin_count(&s, &matches(&s)), 4);
        s.query = TextEdit::new("e");
        assert_eq!(matches(&s), [4], "a search covers every app");
        assert_eq!(pin_count(&s, &matches(&s)), 0, "a search does not put pins first");
    }

    #[test]
    fn parses_desktop_entry() {
        let e = parse("[Desktop Entry]\nType=Application\nName=Foo\nExec=foo --x %U\nTerminal=true\n[Desktop Action n]\nName=Bar", "/a/foo.desktop".into()).unwrap();
        assert_eq!((e.name.as_str(), e.id.as_str(), e.terminal), ("Foo", "foo.desktop", true));
        assert_eq!(e.exec, ["foo", "--x"]);
        assert!(parse("[Desktop Entry]\nName=A\nExec=a\nNoDisplay=true", "x".into()).is_none());
        assert!(parse("[Desktop Entry]\nName=A\nExec=%U", "x".into()).is_none(), "no command left");
        let e = parse("[Desktop Entry]\nName = Foo\nExec = foo\nTerminal = true ", "x".into()).unwrap();
        assert_eq!((e.name.as_str(), e.terminal), ("Foo", true), "spaces around = are ignored");
        assert!(parse("[Desktop Entry]\nName=A\nExec=a\nNoDisplay = true", "x".into()).is_none());
    }

    #[test]
    fn terminal_apps_run_inside_the_terminal() {
        let mut app = Entry { name: "Top".into(), id: "top.desktop".into(), exec: vec!["top".into()], terminal: true, path: "x".into() };
        let term = || vec!["ghostty".to_string(), "-e".to_string()];
        assert_eq!(argv(&app, term()), ["ghostty", "-e", "top"]);
        app.terminal = false;
        assert_eq!(argv(&app, term()), ["top"]);
    }

    #[test]
    fn a_failed_start_keeps_the_reason_and_counts_nothing() {
        let app = Entry { name: "X".into(), id: "x.desktop".into(), exec: vec!["/nonexistent/launcher-test".into()], terminal: false, path: "x".into() };
        let mut s = State {
            query: TextEdit::new(""), usage: Usage::new(), pinned: vec![], picked: 0, apps: vec![app],
            icons: vec![], _cache: None, settings: Settings::default(), show_settings: false, error: None,
        };
        assert!(!start(&mut s, 0));
        assert!(s.error.as_deref().is_some_and(|e| e.starts_with("Could not start /nonexistent/launcher-test")));
        assert!(s.usage.is_empty());
    }

    #[test]
    fn splits_exec_per_spec() {
        let args = |v: &str| exec_args(v, "Foo", Path::new("/a/foo.desktop"));
        assert_eq!(args("printf 100%%"), ["printf", "100%"]);
        assert_eq!(args("foo --url=%u %F"), ["foo", "--url="], "codes in a word go; a lone code drops the argument");
        assert_eq!(args(r#"sh -c "a  b""#), ["sh", "-c", "a  b"], "quotes keep spaces");
        assert_eq!(args(r#""/opt/my app/run" "say \"hi\"" """#), ["/opt/my app/run", "say \"hi\"", ""]);
        assert_eq!(args(r#""a\sb" %c %k"#), ["a b", "Foo", "/a/foo.desktop"]);
        assert_eq!(args(r#"echo "\\\\$x""#), ["echo", r"\$x"], "four backslashes in the file are one in a quoted argument");
    }

    #[test]
    fn grid_rows_keep_the_column() {
        // 2 pins and 6 others in 4 columns: [p0 p1] [r0 r1 r2 r3] [r4 r5].
        let r = rows(2, 8, 4);
        assert_eq!(r, [(0, 2), (2, 4), (6, 2)]);
        assert_eq!(row_step(&r, 0, 1), 2, "down from p0 is r0");
        assert_eq!(row_step(&r, 5, -1), 1, "up from r3 is the last pin");
        assert_eq!(row_step(&r, 7, 1), 1, "down from r5 wraps to p1");
        assert_eq!(row_step(&r, 0, -1), 6, "up from p0 wraps to r4");
        assert_eq!(row_step(&rows(0, 3, 1), 2, 1), 0, "the list steps one app and wraps");
    }

    fn state(names: &[&str], pinned: &[&str], settings: Settings) -> State {
        let app = |n: &&str| Entry { name: n.to_string(), id: format!("{n}.desktop"), exec: vec![], terminal: false, path: format!("{n}.desktop").into() };
        State {
            query: TextEdit::new(""), usage: Usage::new(), pinned: pinned.iter().map(|p| format!("{p}.desktop")).collect(), picked: 0,
            apps: names.iter().map(app).collect(), icons: vec![], _cache: None, settings, show_settings: false, error: None,
        }
    }

    #[test]
    fn pinning_keeps_the_selection_on_the_app() {
        let mut s = state(&["a", "b", "c"], &[], Settings::default());
        s.picked = 2;
        assert!(flip_pin(&mut s));
        assert_eq!((matches(&s), s.picked), (vec![2, 0, 1], 0), "c moves to the top, and the selection goes with it");
        assert!(flip_pin(&mut s));
        assert_eq!((matches(&s), s.picked), (vec![0, 1, 2], 2), "unpinned, c goes back, still picked");
    }

    #[test]
    fn the_highlight_sits_on_the_picked_app() {
        let names = ["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"];
        for grid in [false, true] {
            // Pins a and b, then 8 apps: in the grid's 6 columns that is [a b] [c..h] [i j].
            let s = state(&names, &["a", "b"], Settings { grid, tall: true, ..Settings::default() });
            let mut rt = rune::Runtime::new(s, view, Settings::window(), rune::Env::new(true, 1.0));
            rt.frame(0.0);
            for picked in [0, 1, 2, 5, 8, 9, 0] {
                rt.update(|s, _| s.picked = picked);
                for _ in 0..120 { rt.frame(1.0 / 60.0); }
                let hl = rt.presented("highlight").expect("highlight").0;
                let app = rt.presented(&format!("app-{picked}")).expect("app").0;
                let off = (0..4).map(|k| (hl[k] - app[k]).abs()).fold(0.0, f32::max);
                assert!(off < 0.5, "grid {grid}, app {picked}: highlight {hl:?}, app {app:?}");
            }
        }
    }

    #[test]
    fn pins_drop_blanks_and_repeats() {
        assert_eq!(parse_pinned("a.desktop\n\n b.desktop\na.desktop\n"), ["a.desktop", "b.desktop"]);
    }

    #[test]
    fn data_dirs_ignore_empty_and_relative() {
        let default: Vec<PathBuf> = vec!["/usr/local/share".into(), "/usr/share".into()];
        assert_eq!(data_dirs(None), default);
        assert_eq!(data_dirs(Some(String::new())), default);
        assert_eq!(data_dirs(Some("rel::/nix/share".into())), [PathBuf::from("/nix/share")]);
    }

    #[test]
    fn grid_columns_fit_the_card() {
        for icon in [24.0, 32.0, 48.0] {
            let s = Settings { grid: true, icon, ..Settings::default() };
            let n = s.cols() as f32;
            assert!(n * s.cell() + (n - 1.0) * GAP <= s.card().0 - 2.0 * LIST_PAD, "icon {icon}");
        }
        assert_eq!(Settings::default().cols(), 1);
    }

    #[test]
    fn settings_round_trip() {
        let s = Settings { grid: true, icon: 48.0, names: false, tall: true, corners: Corners::Soft, shadow: 2 };
        assert_eq!(Settings::parse(&s.to_text()), s);
        assert_eq!(Settings::parse("junk\nicon=abc"), Settings::default());
    }
}
