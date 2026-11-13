//! App launcher: lists XDG .desktop apps, type to filter, Enter launches, Escape quits.
//! A click outside the card closes it. If focus stays (Hyprland: `stay_focused`), that click comes
//! here and the card's modal sees it. If not, the click moves focus and the focus loss closes it.
//! With focus-follows-mouse and no `stay_focused`, a pointer move off the window closes it.
//! The gear button or Ctrl+, opens layout settings, saved to `$XDG_CONFIG_HOME/sh.bzn.launcher/settings`.

// cargo-llvm-cov sets `coverage_nightly` only on a nightly rustc; stable builds ignore both
// attributes that use it.
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

use std::{
    cmp::Reverse,
    collections::{HashMap, HashSet},
    env,
    fmt::{self, Write as _},
    fs, io, iter, mem,
    os::unix::{
        fs::{DirBuilderExt as _, FileTypeExt as _, MetadataExt as _, PermissionsExt as _},
        net::UnixDatagram,
        process::CommandExt as _,
    },
    path::{Path, PathBuf},
    process::Command,
    thread,
};

use rune::text::advance;
use rune::tok::{self, TextStyle, type_};
use rune::{
    App, AppSpec, Color, Cursor, Cx, Cycle, El, Image, ImageCache, Key, KeyEvent, ScrollSpec,
    TextEdit, Tx, WindowOptions, WindowSpec, col, el, row, spacer, text, text_input,
};

/// The Wayland `app_id` / X11 class and the config folder name.
const APP_ID: &str = "sh.bzn.launcher";
/// Padding around the results, and the gap between rows and between grid cells.
const LIST_PAD: f32 = 6.0;
const GAP: f32 = 2.0;
/// Space under the last app, and the scroll margin so a picked row is not clipped by the card edge.
const LIST_END: f32 = 12.0;
/// The height of the line between the pins and the other apps, with its margins.
const DIVIDER_H: f32 = 8.5;
/// The size of the search and settings icons in the header.
const HEADER_ICON: f32 = 18.0;
/// The width of every settings control, and the height of a settings row. Two headings, eight
/// rows and the credit fit the compact card.
const CONTROL_W: f32 = 280.0;
const ROW_H: f32 = 40.0;
/// The search field and its placeholder.
const QUERY_FONT: TextStyle = TextStyle::new(17.0, 400.0);
/// Settings labels and buttons, and the settings section headings.
const SETTING_FONT: TextStyle = TextStyle::new(15.0, 400.0);
const SECTION_FONT: TextStyle = TextStyle::new(13.0, 600.0);
/// The block caret: on for half of each period, then off. Reduced motion keeps it on. While
/// only the caret moves, the window wakes 10 times a second.
const CARET_BLINK: Cycle = Cycle {
    period: 1.06,
    phase: 0.0,
    stops: &[(0.0, 1.0), (0.5, 1.0), (0.5, 0.0), (1.0, 0.0)],
    still: 1.0,
    fps: 10.0,
};
/// The pin shortcut hint in the header: its key caps, and the word after them.
const HINT_KEY_FONT: TextStyle = TextStyle::new(14.0, 500.0);
const HINT_FONT: TextStyle = TextStyle::new(15.0, 400.0);
/// Profile links in the settings footer.
const GITHUB: &str = "https://github.com/andybzn";
const X: &str = "https://x.com/andyrbzn";
/// The height of the settings footer: the credit, and the profile links on the right.
const FOOTER_H: f32 = 40.0;

/// One app. `id` is the .desktop file name, the key for pins and launch counts.
struct Entry {
    name: String,
    id: String,
    exec: Vec<String>,
    terminal: bool,
    path: PathBuf,
}

/// An XDG base directory: the variable if it holds an absolute path, else `fallback` under $HOME.
/// The spec says an empty or relative value counts as unset.
fn xdg_dir(var: &str, fallback: &str) -> PathBuf {
    env::var(var)
        .ok()
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| PathBuf::from(env::var("HOME").unwrap_or_default()).join(fallback))
}

/// `$XDG_DATA_DIRS` without its empty and relative entries, or the spec default when none are left.
fn data_dirs(var: Option<String>) -> Vec<PathBuf> {
    let dirs: Vec<PathBuf> = var
        .unwrap_or_default()
        .split(':')
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .collect();
    if dirs.is_empty() {
        vec!["/usr/local/share".into(), "/usr/share".into()]
    } else {
        dirs
    }
}

/// Write a file, making its folder first. A lost setting or launch count is not worth a crash.
fn write_file(path: PathBuf, text: String) {
    let _: Option<io::Result<()>> = path.parent().map(fs::create_dir_all);
    let _: io::Result<()> = fs::write(path, text);
}

/// Corner styles, as in hokum's notification cards.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Corners {
    Sharp,
    Rounded,
    Soft,
    Round,
}

impl Corners {
    const ALL: [Self; 4] = [Self::Sharp, Self::Rounded, Self::Soft, Self::Round];
    /// (window radius, radius of the items inside it).
    const fn radii(self) -> (f32, f32) {
        match self {
            Self::Sharp => (4.0, 4.0),
            Self::Rounded => (10.0, 6.0),
            Self::Soft => (16.0, 7.0),
            Self::Round => (24.0, 10.0),
        }
    }
    const fn name(self) -> &'static str {
        match self {
            Self::Sharp => "sharp",
            Self::Rounded => "rounded",
            Self::Soft => "soft",
            Self::Round => "round",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Settings {
    grid: bool,
    icon: u16,
    /// The app name size, in points.
    text: u8,
    names: bool,
    tall: bool,
    corners: Corners,
    shadow: u8,
    highlight: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            grid: false,
            icon: 24,
            text: 13,
            names: true,
            tall: false,
            corners: Corners::Rounded,
            shadow: 0,
            highlight: 0,
        }
    }
}

/// Highlight colours: the whites and grey, then by hue. Saved by name, so the order can change.
/// Rune's six accents, and four more. Ten 26-point dots fit the control width.
const SWATCHES: usize = 10;
fn swatch(i: usize) -> (&'static str, Color) {
    let accent = |a| tok::accent(a, true);
    match i {
        1 => ("mist", rune::oklch(0.86, 0.01, 250.0)),
        2 => ("graphite", accent(5)),
        3 => ("blue", accent(0)),
        4 => ("teal", rune::oklch(0.72, 0.11, 195.0)),
        5 => ("green", accent(4)),
        6 => ("orange", accent(3)),
        7 => ("red", rune::oklch(0.64, 0.19, 25.0)),
        8 => ("pink", accent(2)),
        9 => ("purple", accent(1)),
        _ => ("white", Color::WHITE),
    }
}

/// WCAG contrast ratio. `Color` is linear, so `luminance` is already the relative luminance.
fn contrast(a: Color, b: Color) -> f32 {
    let (a, b) = (a.luminance(), b.luminance());
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

/// A count as a pixel factor. It stops at 65535; no list gets that long.
fn to_f32(n: usize) -> f32 {
    f32::from(u16::try_from(n).unwrap_or(u16::MAX))
}

/// Text colour on `bg`: the theme's dark or light ink, whichever has more contrast.
/// `highlight_text_is_readable` checks every swatch reaches 4.5:1.
fn ink(bg: Color) -> Color {
    let t = tok::theme(true);
    if contrast(t.bg, bg) >= contrast(t.text, bg) {
        t.bg
    } else {
        t.text
    }
}

impl Settings {
    fn parse(src: &str) -> Self {
        let mut s = Self::default();
        for (k, v) in src.lines().filter_map(|l| l.split_once('=')) {
            match k.trim() {
                "grid" => s.grid = v.trim() == "true",
                "icon" => s.icon = v.trim().parse().unwrap_or(s.icon),
                "text" => s.text = v.trim().parse().unwrap_or(s.text),
                "names" => s.names = v.trim() == "true",
                "tall" => s.tall = v.trim() == "true",
                "corners" => {
                    if let Some(&c) = Corners::ALL.iter().find(|c| c.name() == v.trim()) {
                        s.corners = c;
                    }
                }
                "shadow" => s.shadow = v.trim().parse::<u8>().unwrap_or(0).min(2),
                "highlight" => {
                    s.highlight = (0..SWATCHES)
                        .find(|&i| swatch(i).0 == v.trim())
                        .unwrap_or(0);
                }
                _ => {}
            }
        }
        s
    }
    fn load(path: &Path) -> Self {
        fs::read_to_string(path).map_or_else(|_| Self::default(), |s| Self::parse(&s))
    }
    fn save(&self, path: &Path) {
        write_file(path.to_owned(), self.to_text());
    }
    fn to_text(self) -> String {
        format!(
            "grid={}\nicon={}\ntext={}\nnames={}\ntall={}\ncorners={}\nshadow={}\nhighlight={}\n",
            self.grid,
            self.icon,
            self.text,
            self.names,
            self.tall,
            self.corners.name(),
            self.shadow,
            swatch(self.highlight).0
        )
    }
    /// The card size.
    const fn card(&self) -> (f32, f32) {
        (560.0, if self.tall { 620.0 } else { 510.0 })
    }
    /// The window never resizes: Hyprland ignores a client's resize request. It opens big enough
    /// for the tallest card with the strongest shadow, and the card sits centred inside it.
    fn window() -> (f32, f32) {
        let max = Self {
            tall: true,
            shadow: 2,
            ..Self::default()
        };
        let (w, h) = max.card();
        (
            2.0_f32.mul_add(max.margin(), w),
            2.0_f32.mul_add(max.margin(), h),
        )
    }
    /// Shadow level 0 (none), 1 (soft), 2 (strong) as (y offset, blur, opacity).
    const fn shadow_spec(&self) -> Option<(f32, f32, f32)> {
        match self.shadow {
            1 => Some((6.0, 24.0, 0.35)),
            2 => Some((10.0, 40.0, 0.6)),
            _ => None,
        }
    }
    /// Room around the card so the shadow is not cut off at the window edge.
    fn margin(&self) -> f32 {
        self.shadow_spec().map_or(0.0, |(y, blur, _)| y + blur)
    }
    /// Grid cell width, and how many cells (with the gaps between them) fit in a row.
    fn cell(&self) -> f32 {
        f32::from(self.icon) + 64.0
    }
    /// The fixed height of one result row: a list row, or a grid cell with its icon and name.
    fn row_h(&self) -> f32 {
        let icon = f32::from(self.icon);
        if !self.grid {
            return icon + 12.0;
        }
        icon + 20.0
            + if self.names {
                f32::from(self.text).mul_add(1.25, 7.0)
            } else {
                0.0
            }
    }
    fn cols(&self) -> usize {
        if !self.grid {
            return 1;
        }
        // Each further cell takes its width and one gap.
        let room = 2.0_f32.mul_add(-LIST_PAD, self.card().0);
        iter::successors(Some(self.cell()), |w| Some(w + GAP + self.cell()))
            .take_while(|&w| w <= room)
            .count()
            .max(1)
    }
}

/// The launcher's own files. `xdg` gives the real places; tests use a temporary folder.
struct Files {
    settings: PathBuf,
    /// Pinned apps, one .desktop file name per line.
    pinned: PathBuf,
    usage: PathBuf,
}

impl Files {
    /// Settings and pins in `$XDG_CONFIG_HOME/sh.bzn.launcher`, launch counts in
    /// `$XDG_STATE_HOME/sh.bzn.launcher`.
    fn xdg() -> Self {
        Self::in_dirs(
            &xdg_dir("XDG_CONFIG_HOME", ".config").join(APP_ID),
            &xdg_dir("XDG_STATE_HOME", ".local/state").join(APP_ID),
        )
    }
    fn in_dirs(config: &Path, state: &Path) -> Self {
        Self {
            settings: config.join("settings"),
            pinned: config.join("pinned"),
            usage: state.join("usage"),
        }
    }
}

/// Launch counts by .desktop file name.
type Usage = HashMap<String, u32>;

fn load_usage(path: &Path) -> Usage {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let (n, id) = l.split_once('\t')?;
            Some((id.to_owned(), n.parse().ok()?))
        })
        .collect()
}

fn save_usage(path: &Path, u: &Usage) {
    write_file(
        path.to_owned(),
        u.iter().fold(String::new(), |mut out, (id, n)| {
            let _: fmt::Result = writeln!(out, "{n}\t{id}");
            out
        }),
    );
}

/// The pins, without blank lines and repeats (a hand-edited file can have both).
fn parse_pinned(src: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    src.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && seen.insert(*l))
        .map(String::from)
        .collect()
}

/// Pin the picked app, or unpin it. The list reorders, so the selection stays on the same app,
/// or goes to the top if the app left the list. False when no app is picked.
fn flip_pin(s: &mut State) -> bool {
    let picked = matches(s)
        .get(s.picked)
        .and_then(|&i| Some((i, s.apps.get(i)?.id.clone())));
    let Some((i, id)) = picked else {
        return false;
    };
    match s.pinned.iter().position(|p| *p == id) {
        Some(at) => {
            s.pinned.remove(at);
        }
        None => s.pinned.push(id),
    }
    s.picked = matches(s).iter().position(|&j| j == i).unwrap_or(0);
    true
}

/// Ctrl+P: flip the pin, save the pins, and bring the app into view in its new place.
fn toggle_pin(s: &mut State, tx: &mut Tx) {
    if !flip_pin(s) {
        return;
    }
    write_file(
        s.files.pinned.clone(),
        s.pinned.iter().flat_map(|id| [id.as_str(), "\n"]).collect(),
    );
    show_picked(s, &matches(s), tx);
}

struct State {
    query: TextEdit,
    usage: Usage,
    pinned: Vec<String>,
    picked: usize,
    apps: Vec<Entry>,
    icons: Vec<Image>,
    /// Kept so the icon worker lives as long as the app.
    cache: Option<ImageCache>,
    settings: Settings,
    show_settings: bool,
    /// Why the last launch failed. Typing clears it.
    error: Option<String>,
    files: Files,
    /// The window had focus once. The compositor can report a loss before the first gain.
    had_focus: bool,
    /// A click outside the card. The next frame closes the window.
    click_away: bool,
}

impl State {
    /// The start-up state: the saved settings, pins and launch counts in `files`, and `apps`.
    fn load(files: Files, apps: Vec<Entry>) -> Self {
        let mut query = TextEdit::new("");
        query.focused = true;
        Self {
            query,
            usage: load_usage(&files.usage),
            pinned: parse_pinned(&fs::read_to_string(&files.pinned).unwrap_or_default()),
            picked: 0,
            apps,
            icons: vec![],
            cache: None,
            settings: Settings::load(&files.settings),
            show_settings: false,
            error: None,
            files,
            had_focus: false,
            click_away: false,
        }
    }
}

fn parse(src: &str, path: PathBuf) -> Option<Entry> {
    let (mut name, mut exec, mut terminal, mut in_main) = (None, None, false, false);
    for line in src.lines() {
        if line.starts_with('[') {
            in_main = line == "[Desktop Entry]";
            continue;
        }
        if !in_main {
            continue;
        }
        // The spec ignores spaces around the `=`; trailing spaces would break `Terminal=true `.
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        match (k.trim_end(), v.trim()) {
            ("Name", v) => name = Some(v.to_owned()),
            ("Exec", v) => exec = Some(v.to_owned()),
            ("Terminal", v) => terminal = v == "true",
            ("NoDisplay" | "Hidden", "true") => return None,
            ("Type", v) if v != "Application" => return None,
            _ => {}
        }
    }
    let name = name?;
    let exec = exec_args(&exec?, &name, &path);
    if exec.is_empty() {
        return None;
    }
    let id = path.file_name()?.to_string_lossy().into_owned();
    Some(Entry {
        name,
        id,
        exec,
        terminal,
        path,
    })
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
        if c != '\\' {
            src.push(c);
            continue;
        }
        match it.next() {
            Some('s') => src.push(' '),
            Some('n') => src.push('\n'),
            Some('t') => src.push('\t'),
            Some('r') => src.push('\r'),
            Some('\\') | None => src.push('\\'),
            Some(o) => {
                src.push('\\');
                src.push(o);
            }
        }
    }
    // `open`: an argument has started, so an empty "" still counts and a lone %U does not.
    let (mut args, mut cur, mut open, mut quoted) = (vec![], String::new(), false, false);
    let mut chars = src.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                quoted = !quoted;
                open = true;
            }
            '\\' if quoted => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            ' ' | '\t' | '\n' if !quoted => {
                if open {
                    args.push(mem::take(&mut cur));
                    open = false;
                }
            }
            '%' => match chars.next() {
                Some('%') => {
                    cur.push('%');
                    open = true;
                }
                Some('c') => {
                    cur.push_str(name);
                    open = true;
                }
                Some('k') => {
                    cur.push_str(&path.to_string_lossy());
                    open = true;
                }
                _ => {}
            },
            _ => {
                cur.push(c);
                open = true;
            }
        }
    }
    if open {
        args.push(cur);
    }
    args
}

/// Where .desktop files live: `$XDG_DATA_HOME`, then each of `$XDG_DATA_DIRS`.
fn app_dirs() -> Vec<PathBuf> {
    iter::once(xdg_dir("XDG_DATA_HOME", ".local/share"))
        .chain(data_dirs(env::var("XDG_DATA_DIRS").ok()))
        .collect()
}

/// The apps in the `applications` folder of each of `dirs`.
fn load_apps(dirs: &[PathBuf]) -> Vec<Entry> {
    let mut seen = HashSet::new();
    let mut apps: Vec<Entry> = dirs
        .iter()
        .flat_map(|d| {
            fs::read_dir(d.join("applications"))
                .into_iter()
                .flatten()
                .flatten()
        })
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
        let pinned = s
            .pinned
            .iter()
            .filter_map(|id| s.apps.iter().position(|a| a.id == *id));
        let mut rest: Vec<(usize, &Entry)> = s
            .apps
            .iter()
            .enumerate()
            .filter(|(_, a)| !s.pinned.contains(&a.id))
            .collect();
        rest.sort_by_key(|(_, a)| Reverse(s.usage.get(&a.id).copied().unwrap_or(0)));
        return pinned.chain(rest.into_iter().map(|(i, _)| i)).collect();
    }
    s.apps
        .iter()
        .enumerate()
        .filter(|(_, a)| a.name.to_lowercase().contains(&q))
        .map(|(i, _)| i)
        .collect()
}

/// How many of `found` are pins. Only an empty query puts the pins first.
fn pin_count(s: &State, found: &[usize]) -> usize {
    if !s.query.text.is_empty() {
        return 0;
    }
    found
        .iter()
        .take_while(|&&i| s.apps.get(i).is_some_and(|a| s.pinned.contains(&a.id)))
        .count()
}

/// The result rows as (first index, length). The pins fill their own rows, so the rest starts
/// on a new row.
fn rows(pins: usize, total: usize, cols: usize) -> Vec<(usize, usize)> {
    let run = |from: usize, to: usize| {
        (from..to)
            .step_by(cols)
            .map(move |r| (r, cols.min(to.saturating_sub(r))))
    };
    run(0, pins).chain(run(pins, total)).collect()
}

/// The (row, column) of item `picked` in `rows`.
fn locate(rows: &[(usize, usize)], picked: usize) -> Option<(usize, usize)> {
    rows.iter().enumerate().find_map(|(r, &(start, len))| {
        picked
            .checked_sub(start)
            .filter(|&c| c < len)
            .map(|c| (r, c))
    })
}

/// Step `i` by `d` (1 or -1) in `0..n`, wrapping at the ends.
fn wrap(i: usize, d: isize, n: usize) -> usize {
    i.checked_add_signed(d)
        .unwrap_or_else(|| n.saturating_sub(1))
        .checked_rem(n)
        .unwrap_or(0)
}

/// Up/Down: move one row, wrapping at the ends. Keep the column, or take the last cell of a short row.
fn row_step(rows: &[(usize, usize)], picked: usize, d: isize) -> usize {
    let next = locate(rows, picked)
        .and_then(|(r, column)| Some((column, rows.get(wrap(r, d, rows.len()))?)));
    let Some((column, &(start, len))) = next else {
        return 0;
    };
    start.saturating_add(column.min(len.saturating_sub(1)))
}

/// The terminal for Terminal=true apps: `$TERMINAL` (`var`), else xdg-terminal-exec. Both must
/// take `-e command`.
fn terminal(var: Option<String>) -> Vec<String> {
    let t = var
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| "xdg-terminal-exec".into());
    t.split_whitespace()
        .map(String::from)
        .chain(["-e".to_owned()])
        .collect()
}

/// The command for an app: its Exec, inside `term` when it is a Terminal=true app.
fn argv(app: &Entry, term: Vec<String>) -> Vec<String> {
    if app.terminal {
        term.into_iter().chain(app.exec.iter().cloned()).collect()
    } else {
        app.exec.clone()
    }
}

/// Start app `i`. If it starts, count the launch. If not, keep the reason in `s.error`.
fn start(s: &mut State, i: usize) -> bool {
    let Some(app) = s.apps.get(i) else {
        return false;
    };
    let argv = argv(app, terminal(env::var("TERMINAL").ok()));
    // `parse` drops apps with no command, so `argv` is never empty.
    let Some((cmd, rest)) = argv.split_first() else {
        return false;
    };
    // Its own process group, so the app outlives the launcher and gets none of its signals.
    if let Err(e) = Command::new(cmd).args(rest).process_group(0).spawn() {
        s.error = Some(format!("Could not start {cmd}: {e}"));
        return false;
    }
    let n = s.usage.entry(app.id.clone()).or_default();
    *n = n.saturating_add(1);
    save_usage(&s.files.usage, &s.usage);
    true
}

/// Enter or a click: start the picked app and close. A failed start keeps the window open.
fn launch(s: &mut State, tx: &mut Tx) {
    // The settings panel hides the list, so Enter there must not start an app.
    if s.show_settings {
        return;
    }
    let Some(&i) = matches(s).get(s.picked) else {
        return;
    };
    if start(s, i) {
        tx.close_window();
    }
}

/// Move the selection and scroll it into view. Up/Down move by row, Left/Right by item; both wrap.
fn step(s: &mut State, d: isize, by_row: bool, tx: &mut Tx) {
    let found = matches(s);
    if found.is_empty() {
        return;
    }
    s.picked = if by_row {
        row_step(
            &rows(pin_count(s, &found), found.len(), s.settings.cols()),
            s.picked,
            d,
        )
    } else {
        wrap(s.picked, d, found.len())
    };
    show_picked(s, &found, tx);
}

/// Scroll the picked app into view, and let the highlight slide to it.
fn show_picked(s: &State, found: &[usize], tx: &mut Tx) {
    if let Some(i) = found.get(s.picked) {
        tx.scroll_into_view(format!("app-{i}"));
    }
    // Keys in the search field can count as typing, which lands at once; the highlight must slide.
    tx.animated();
}

/// The window gained or lost focus. A loss after the first gain is a click away, so close.
fn on_focus(s: &mut State, focused: bool, tx: &mut Tx) {
    if closes_on_focus(s, focused) {
        tx.close_window();
    }
}

/// Whether this focus change closes the window. Records the first gain.
const fn closes_on_focus(s: &mut State, focused: bool) -> bool {
    let close = s.had_focus && !focused;
    s.had_focus |= focused;
    close
}

/// A click on a button takes focus; give it back to the search field so the keys keep working.
const fn toggle_settings(s: &mut State) {
    s.show_settings = !s.show_settings;
    s.query.focused = true;
}

/// The app's icon, or a gradient circle with its initial when it has none.
fn icon(s: &State, i: usize) -> El {
    let size = f32::from(s.settings.icon);
    // An icon can be missing before setup fills the list (and in tests).
    if let Some(img) = s.icons.get(i).filter(|img| img.error().is_none()) {
        return el().size(size, size).pixels(img.pixels());
    }
    let name = s.apps.get(i).map_or("", |a| a.name.as_str());
    let initial = name
        .chars()
        .next()
        .unwrap_or('?')
        .to_uppercase()
        .to_string();
    // The name picks the hue, so each app keeps its colour between runs.
    let h = name.bytes().map(usize::from).sum::<usize>();
    let font = if size >= 32.0 {
        type_::TITLE
    } else {
        type_::LABEL
    };
    el().size(size, size)
        .radius(size / 2.0)
        .center()
        .gradient(tok::accent(h, true), tok::accent(h.wrapping_add(1), true))
        .child(text(initial).font(font).color(Color::WHITE))
}

fn results(s: &State) -> El {
    let t = tok::theme(true);
    let set = s.settings;
    let found = matches(s);
    // scroll_margin keeps the highlight clear of the card edge when the picked row is the last in view.
    let mut list = col()
        .key("results")
        .grow(1.0)
        .min_h(0.0)
        .padding(LIST_PAD)
        .gap(GAP)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .scroll_margin(LIST_END);
    // Before you type, the pins lead the list; a faint line divides them from the rest, and in
    // the grid the rest starts a new row.
    let pins = pin_count(s, &found);
    let divider = pins.div_ceil(set.cols());
    let rows = rows(pins, found.len(), set.cols());
    let hl_color = swatch(set.highlight).1;
    // One highlight under the picked app. It is one element at a calculated place, so it
    // slides on its spring when the selection moves. It is the first child, so the apps draw on it.
    // ponytail: the place comes from the fixed row heights; a row that sizes to its content breaks it.
    if let Some((r, column)) = locate(&rows, s.picked) {
        let y = to_f32(r).mul_add(set.row_h() + GAP, LIST_PAD)
            + if divider > 0 && r >= divider {
                DIVIDER_H + GAP
            } else {
                0.0
            };
        let hl = el()
            .key("highlight")
            .h(set.row_h())
            .radius(set.corners.radii().1)
            .fill(hl_color)
            .token_motion(tok::motion::SNAP)
            .a11y_hidden();
        list = list.child(if set.grid {
            hl.w(set.cell())
                .absolute(to_f32(column).mul_add(set.cell() + GAP, LIST_PAD), y)
        } else {
            hl.pin(Some(LIST_PAD), Some(y), Some(LIST_PAD), None)
        });
    }
    for (n, (start, len)) in rows.into_iter().enumerate() {
        if n == divider && divider > 0 {
            list = list.child(
                el().h(0.5)
                    .mx(64.0)
                    .my((DIVIDER_H - 0.5) / 2.0)
                    .fill(t.separator.alpha(0.07)),
            );
        }
        let mut line = row().gap(GAP);
        for (pos, &i) in found.iter().enumerate().skip(start).take(len) {
            let Some(app) = s.apps.get(i) else { continue };
            let on = pos == s.picked;
            let ink = if on { ink(hl_color) } else { t.text };
            // The highlighted name is bold: the name size at a heavier weight.
            let font = TextStyle::new(f32::from(set.text), if on { 600.0 } else { 400.0 });
            let name = text(app.name.clone())
                .font(font)
                .color(ink)
                .single_line()
                .truncate();
            let item = if set.grid {
                col().w(set.cell()).h(set.row_h()).px(4.0).gap(6.0).center()
            } else {
                row().w_full().h(set.row_h()).px(12.0).gap(10.0).center_y()
            };
            let mut item = item
                .key(format!("app-{i}"))
                .cursor(Cursor::Pointer)
                .role("button")
                .label(app.name.clone())
                .on_press_tx(move |state: &mut State, tx: &mut Tx| {
                    state.picked = pos;
                    state.query.focused = true;
                    launch(state, tx);
                })
                .radius(set.corners.radii().1)
                .child(icon(s, i));
            if set.names {
                item = item.child(name);
            }
            // A pin mark: after the name in the list, in the corner of the cell in the grid.
            if s.pinned.contains(&app.id) {
                let pin = pin_icon(if on {
                    ink.alpha(0.45)
                } else {
                    t.text_secondary.alpha(0.6)
                });
                item = if set.grid {
                    item.child(pin.absolute(set.cell() - 18.0, 6.0))
                } else {
                    item.child(spacer()).child(pin)
                };
            }
            line = line.child(item);
        }
        list = list.child(line);
    }
    // Room under the last app, so it does not touch the bottom edge of the card.
    list.child(el().h(LIST_END))
}

/// A settings button: its name, whether it is selected, and the change it makes.
type Choice = (&'static str, bool, fn(&mut Settings));

/// One settings row: a label and a segmented control. All controls share one width and split
/// it evenly, so their edges line up down the pane.
fn choice(label: &str, options: Vec<Choice>) -> El {
    let t = tok::theme(true);
    let (pad, gap) = (2.0_f32, 2.0_f32);
    let n = to_f32(options.len());
    let seg_w = gap.mul_add(-(n - 1.0), 2.0_f32.mul_add(-pad, CONTROL_W)) / n;
    let mut buttons = row()
        .w(CONTROL_W)
        .gap(gap)
        .padding(pad)
        .radius(8.0)
        .fill(t.control_track);
    for (name, on, apply) in options {
        buttons = buttons.child(
            el().key(format!("set-{label}-{name}"))
                .cursor(Cursor::Pointer)
                .role("button")
                .label(name)
                .w(seg_w)
                .h(26.0)
                .radius(6.0)
                .center()
                .fill(if on { t.indicator } else { Color::TRANSPARENT })
                .hover(t.hover)
                .press(t.press)
                .child(
                    text(name)
                        .font(if on {
                            TextStyle::new(SETTING_FONT.size, 500.0)
                        } else {
                            SETTING_FONT
                        })
                        .color(if on { t.text } else { t.text_secondary }),
                )
                .on_press(move |s: &mut State| {
                    apply(&mut s.settings);
                    s.settings.save(&s.files.settings);
                    s.query.focused = true;
                }),
        );
    }
    setting_row(label, buttons)
}

/// A settings row: a label, and its control on the right.
fn setting_row(label: &str, control: El) -> El {
    let t = tok::theme(true);
    row()
        .w_full()
        .h(ROW_H)
        .px(18.0)
        .center_y()
        .child(text(label.to_owned()).font(SETTING_FONT).color(t.text))
        .child(spacer())
        .child(control)
}

/// A group heading over related rows.
fn section(name: &str) -> El {
    let t = tok::theme(true);
    row().w_full().px(18.0).pt(20.0).pb(6.0).child(
        text(name.to_owned())
            .font(SECTION_FONT)
            .color(t.text_secondary),
    )
}

/// The highlight colour picker: one dot per swatch, spread across the control width, a ring
/// round the picked one.
fn swatches(picked: usize) -> El {
    let t = tok::theme(true);
    let mut dots = row().w(CONTROL_W).justify_between().center_y();
    for i in 0..SWATCHES {
        let (name, c) = swatch(i);
        dots = dots.child(
            el().key(format!("hl-{name}"))
                .cursor(Cursor::Pointer)
                .role("button")
                .label(name)
                .size(26.0, 26.0)
                .radius(13.0)
                .center()
                .border(
                    1.5,
                    if i == picked {
                        t.text
                    } else {
                        Color::TRANSPARENT
                    },
                )
                .child(el().size(18.0, 18.0).radius(9.0).fill(c))
                .on_press(move |s: &mut State| {
                    s.settings.highlight = i;
                    s.settings.save(&s.files.settings);
                    s.query.focused = true;
                }),
        );
    }
    setting_row("Highlight", dots)
}

fn settings_view(state: &State) -> El {
    let set = state.settings;
    col()
        .key("settings")
        .grow(1.0)
        .child(section("Results"))
        .child(choice(
            "Layout",
            vec![
                ("List", !set.grid, |s| s.grid = false),
                ("Grid", set.grid, |s| s.grid = true),
            ],
        ))
        .child(choice(
            "Icon size",
            vec![
                ("Small", set.icon == 24, |s| s.icon = 24),
                ("Medium", set.icon == 32, |s| s.icon = 32),
                ("Large", set.icon == 48, |s| s.icon = 48),
            ],
        ))
        .child(choice(
            "Name size",
            vec![
                ("Small", set.text == 13, |s| s.text = 13),
                ("Medium", set.text == 15, |s| s.text = 15),
                ("Large", set.text == 17, |s| s.text = 17),
            ],
        ))
        .child(choice(
            "Names",
            vec![
                ("Show", set.names, |s| s.names = true),
                ("Hide", !set.names, |s| s.names = false),
            ],
        ))
        .child(section("Appearance"))
        .child(choice(
            "Height",
            vec![
                ("Compact", !set.tall, |s| s.tall = false),
                ("Tall", set.tall, |s| s.tall = true),
            ],
        ))
        .child(choice(
            "Corners",
            vec![
                ("Sharp", set.corners == Corners::Sharp, |s| {
                    s.corners = Corners::Sharp;
                }),
                ("Rounded", set.corners == Corners::Rounded, |s| {
                    s.corners = Corners::Rounded;
                }),
                ("Soft", set.corners == Corners::Soft, |s| {
                    s.corners = Corners::Soft;
                }),
                ("Round", set.corners == Corners::Round, |s| {
                    s.corners = Corners::Round;
                }),
            ],
        ))
        .child(choice(
            "Shadow",
            vec![
                ("None", set.shadow == 0, |s| s.shadow = 0),
                ("Soft", set.shadow == 1, |s| s.shadow = 1),
                ("Strong", set.shadow == 2, |s| s.shadow = 2),
            ],
        ))
        .child(swatches(set.highlight))
        .child(spacer())
        .child(
            row().w_full().h(FOOTER_H).center().child(credit()).child(
                row()
                    .gap(2.0)
                    .child(social("github", GITHUB, "andybzn on GitHub", &GITHUB_LOGO))
                    .child(social("x", X, "andyrbzn on X", &X_LOGO))
                    .pin(None, Some((FOOTER_H - 24.0) / 2.0), Some(12.0), None),
            ),
        )
}

/// "launcher · © andybzn", centred at the bottom of the settings.
fn credit() -> El {
    let muted = tok::theme(true).text_secondary;
    row()
        .key("credit")
        .gap(4.0)
        .center_y()
        .child(
            text("launcher ·".to_owned())
                .font(type_::LABEL)
                .color(muted),
        )
        .child(rune::icon(&COPYRIGHT, 13.0).color(muted))
        .child(text("andybzn".to_owned()).font(type_::LABEL).color(muted))
}

/// A profile link: its mark and an arrow, in the bottom-right corner of the settings.
fn social(key: &str, url: &str, label: &str, mark: &'static rune::Icon) -> El {
    let t = tok::theme(true);
    row()
        .key(key)
        .cursor(Cursor::Pointer)
        .h(24.0)
        .px(6.0)
        .gap(2.0)
        .radius(6.0)
        .center_y()
        .hover(t.hover)
        .href(url)
        .label(label)
        .child(rune::icon(mark, 16.0).color(t.text_secondary))
        .child(rune::icon(&ARROW_UP_RIGHT, 12.0).color(t.text_secondary))
}

// Phosphor Icons (regular, 256 grid), MIT © Phosphor Icons. Drawn in the element's colour.
static PIN: rune::Icon = rune::Icon {
    name: "push-pin",
    view: 256.0,
    body: r#"<path d="M235.32,81.37,174.63,20.69a16,16,0,0,0-22.63,0L98.37,74.49c-10.66-3.34-35-7.37-60.4,13.14a16,16,0,0,0-1.29,23.78L85,159.71,42.34,202.34a8,8,0,0,0,11.32,11.32L96.29,171l48.29,48.29A16,16,0,0,0,155.9,224c.38,0,.75,0,1.13,0a15.93,15.93,0,0,0,11.64-6.33c19.64-26.1,17.75-47.32,13.19-60L235.33,104A16,16,0,0,0,235.32,81.37ZM224,92.69h0l-57.27,57.46a8,8,0,0,0-1.49,9.22c9.46,18.93-1.8,38.59-9.34,48.62L48,100.08c12.08-9.74,23.64-12.31,32.48-12.31A40.13,40.13,0,0,1,96.81,91a8,8,0,0,0,9.25-1.51L163.32,32,224,92.68Z" fill="currentColor"/>"#,
};

static SEARCH: rune::Icon = rune::Icon {
    name: "magnifying-glass",
    view: 256.0,
    body: r#"<path d="M229.66,218.34l-50.07-50.06a88.11,88.11,0,1,0-11.31,11.31l50.06,50.07a8,8,0,0,0,11.32-11.32ZM40,112a72,72,0,1,1,72,72A72.08,72.08,0,0,1,40,112Z" fill="currentColor"/>"#,
};
static SETTINGS: rune::Icon = rune::Icon {
    name: "gear",
    view: 256.0,
    body: r#"<path d="M128,80a48,48,0,1,0,48,48A48.05,48.05,0,0,0,128,80Zm0,80a32,32,0,1,1,32-32A32,32,0,0,1,128,160Zm88-29.84q.06-2.16,0-4.32l14.92-18.64a8,8,0,0,0,1.48-7.06,107.21,107.21,0,0,0-10.88-26.25,8,8,0,0,0-6-3.93l-23.72-2.64q-1.48-1.56-3-3L186,40.54a8,8,0,0,0-3.94-6,107.71,107.71,0,0,0-26.25-10.87,8,8,0,0,0-7.06,1.49L130.16,40Q128,40,125.84,40L107.2,25.11a8,8,0,0,0-7.06-1.48A107.6,107.6,0,0,0,73.89,34.51a8,8,0,0,0-3.93,6L67.32,64.27q-1.56,1.49-3,3L40.54,70a8,8,0,0,0-6,3.94,107.71,107.71,0,0,0-10.87,26.25,8,8,0,0,0,1.49,7.06L40,125.84Q40,128,40,130.16L25.11,148.8a8,8,0,0,0-1.48,7.06,107.21,107.21,0,0,0,10.88,26.25,8,8,0,0,0,6,3.93l23.72,2.64q1.49,1.56,3,3L70,215.46a8,8,0,0,0,3.94,6,107.71,107.71,0,0,0,26.25,10.87,8,8,0,0,0,7.06-1.49L125.84,216q2.16.06,4.32,0l18.64,14.92a8,8,0,0,0,7.06,1.48,107.21,107.21,0,0,0,26.25-10.88,8,8,0,0,0,3.93-6l2.64-23.72q1.56-1.48,3-3L215.46,186a8,8,0,0,0,6-3.94,107.71,107.71,0,0,0,10.87-26.25,8,8,0,0,0-1.49-7.06Zm-16.1-6.5a73.93,73.93,0,0,1,0,8.68,8,8,0,0,0,1.74,5.48l14.19,17.73a91.57,91.57,0,0,1-6.23,15L187,173.11a8,8,0,0,0-5.1,2.64,74.11,74.11,0,0,1-6.14,6.14,8,8,0,0,0-2.64,5.1l-2.51,22.58a91.32,91.32,0,0,1-15,6.23l-17.74-14.19a8,8,0,0,0-5-1.75h-.48a73.93,73.93,0,0,1-8.68,0,8,8,0,0,0-5.48,1.74L100.45,215.8a91.57,91.57,0,0,1-15-6.23L82.89,187a8,8,0,0,0-2.64-5.1,74.11,74.11,0,0,1-6.14-6.14,8,8,0,0,0-5.1-2.64L46.43,170.6a91.32,91.32,0,0,1-6.23-15l14.19-17.74a8,8,0,0,0,1.74-5.48,73.93,73.93,0,0,1,0-8.68,8,8,0,0,0-1.74-5.48L40.2,100.45a91.57,91.57,0,0,1,6.23-15L69,82.89a8,8,0,0,0,5.1-2.64,74.11,74.11,0,0,1,6.14-6.14A8,8,0,0,0,82.89,69L85.4,46.43a91.32,91.32,0,0,1,15-6.23l17.74,14.19a8,8,0,0,0,5.48,1.74,73.93,73.93,0,0,1,8.68,0,8,8,0,0,0,5.48-1.74L155.55,40.2a91.57,91.57,0,0,1,15,6.23L173.11,69a8,8,0,0,0,2.64,5.1,74.11,74.11,0,0,1,6.14,6.14,8,8,0,0,0,5.1,2.64l22.58,2.51a91.32,91.32,0,0,1,6.23,15l-14.19,17.74A8,8,0,0,0,199.87,123.66Z" fill="currentColor"/>"#,
};
static COPYRIGHT: rune::Icon = rune::Icon {
    name: "copyright",
    view: 256.0,
    body: r#"<path d="M128,24A104,104,0,1,0,232,128,104.11,104.11,0,0,0,128,24Zm0,192a88,88,0,1,1,88-88A88.1,88.1,0,0,1,128,216ZM96,128a32,32,0,0,0,57.6,19.2,8,8,0,0,1,12.8,9.61,48,48,0,1,1,0-57.62,8,8,0,0,1-12.8,9.61A32,32,0,0,0,96,128Z" fill="currentColor"/>"#,
};
static GITHUB_LOGO: rune::Icon = rune::Icon {
    name: "github-logo",
    view: 256.0,
    body: r#"<path d="M208.31,75.68A59.78,59.78,0,0,0,202.93,28,8,8,0,0,0,196,24a59.75,59.75,0,0,0-48,24H124A59.75,59.75,0,0,0,76,24a8,8,0,0,0-6.93,4,59.78,59.78,0,0,0-5.38,47.68A58.14,58.14,0,0,0,56,104v8a56.06,56.06,0,0,0,48.44,55.47A39.8,39.8,0,0,0,96,192v8H72a24,24,0,0,1-24-24A40,40,0,0,0,8,136a8,8,0,0,0,0,16,24,24,0,0,1,24,24,40,40,0,0,0,40,40H96v16a8,8,0,0,0,16,0V192a24,24,0,0,1,48,0v40a8,8,0,0,0,16,0V192a39.8,39.8,0,0,0-8.44-24.53A56.06,56.06,0,0,0,216,112v-8A58.14,58.14,0,0,0,208.31,75.68ZM200,112a40,40,0,0,1-40,40H112a40,40,0,0,1-40-40v-8a41.74,41.74,0,0,1,6.9-22.48A8,8,0,0,0,80,73.83a43.81,43.81,0,0,1,.79-33.58,43.88,43.88,0,0,1,32.32,20.06A8,8,0,0,0,119.82,64h32.35a8,8,0,0,0,6.74-3.69,43.87,43.87,0,0,1,32.32-20.06A43.81,43.81,0,0,1,192,73.83a8.09,8.09,0,0,0,1,7.65A41.72,41.72,0,0,1,200,104Z" fill="currentColor"/>"#,
};
static X_LOGO: rune::Icon = rune::Icon {
    name: "x-logo",
    view: 256.0,
    body: r#"<path d="M214.75,211.71l-62.6-98.38,61.77-67.95a8,8,0,0,0-11.84-10.76L143.24,99.34,102.75,35.71A8,8,0,0,0,96,32H48a8,8,0,0,0-6.75,12.3l62.6,98.37-61.77,68a8,8,0,1,0,11.84,10.76l58.84-64.72,40.49,63.63A8,8,0,0,0,160,224h48a8,8,0,0,0,6.75-12.29ZM164.39,208,62.57,48h29L193.43,208Z" fill="currentColor"/>"#,
};
static ARROW_UP_RIGHT: rune::Icon = rune::Icon {
    name: "arrow-up-right",
    view: 256.0,
    body: r#"<path d="M200,64V168a8,8,0,0,1-16,0V83.31L69.66,197.66a8,8,0,0,1-11.32-11.32L172.69,72H88a8,8,0,0,1,0-16H192A8,8,0,0,1,200,64Z" fill="currentColor"/>"#,
};

fn pin_icon(c: Color) -> El {
    rune::icon(&PIN, 14.0).color(c)
}

/// A keyboard key, for a shortcut hint.
fn key_cap(name: &str) -> El {
    let t = tok::theme(true);
    el().h(24.0)
        .px(7.0)
        .radius(6.0)
        .fill(t.control_track)
        .border(1.0, t.control_off)
        .center()
        .child(
            text(name.to_owned())
                .font(HINT_KEY_FONT)
                .color(t.text_secondary),
        )
}

/// The header field: search, or an empty "settings" placeholder while the panel is open.
fn search_field(s: &State, edit: &TextEdit) -> El {
    let t = tok::theme(true);
    text_input(edit, QUERY_FONT.lh(26.0))
        .key("query")
        .single_line()
        .grow(1.0)
        .color(t.text)
        // The view draws a hollow box in place of the caret line. The settings show no selection.
        .caret(
            Color::TRANSPARENT,
            if s.show_settings {
                Color::TRANSPARENT
            } else {
                t.text.alpha(0.2)
            },
        )
        .placeholder(
            if s.show_settings {
                "settings"
            } else {
                // Spaces clear the block caret, which sits at the start of the empty field.
                "   search apps"
            },
            t.text_secondary,
        )
        .on_edit(|s: &mut State| &mut s.query)
        .on_input_key(|s: &mut State, ev: &KeyEvent, tx: &mut Tx| {
            match ev.key {
                _ if ev.is_command(',') => toggle_settings(s),
                _ if ev.is_command('p') && !s.show_settings => toggle_pin(s, tx),
                Key::Escape if s.show_settings => s.show_settings = false,
                Key::Escape => tx.close_window(),
                // Settings is not a search field: keep the query, show the "settings" placeholder.
                _ if s.show_settings => {}
                Key::Down => step(s, 1, true, tx),
                Key::Up => step(s, -1, true, tx),
                // Left and Right move the text caret in the list; in the grid they move the selection.
                Key::Right if s.settings.grid => step(s, 1, false, tx),
                Key::Left if s.settings.grid => step(s, -1, false, tx),
                _ => return false,
            }
            true
        })
        .on_input_change(|s: &mut State, _| {
            s.picked = 0;
            s.error = None;
        })
        .on_submit(launch)
}

fn view(s: &State, cx: &mut Cx<State>) -> El {
    let t = tok::theme(true);
    // Empty while settings show, so the placeholder can read "settings" over any query.
    let mut settings_bar = TextEdit::new("");
    settings_bar.focused = s.query.focused;
    let field = search_field(
        s,
        if s.show_settings {
            &settings_bar
        } else {
            &s.query
        },
    );
    let gear = el()
        .key("gear")
        .cursor(Cursor::Pointer)
        .role("button")
        .label("Settings")
        .size(32.0, 32.0)
        .radius(8.0)
        .center()
        .fill(if s.show_settings {
            t.control_off
        } else {
            Color::TRANSPARENT
        })
        .child(rune::icon(&SETTINGS, HEADER_ICON).color(t.text_secondary))
        .on_press(toggle_settings);
    let set = s.settings;
    let radius = set.corners.radii().0;
    let mut header = row().h(56.0).pl(18.0).pr(12.0).gap(12.0).center_y();
    // The settings are not a search: no magnifying glass and no caret. The field keeps the
    // focus, so Escape and Ctrl+, still reach it.
    header = if s.show_settings {
        header.child(field)
    } else {
        header
            .child(rune::icon(&SEARCH, HEADER_ICON).color(t.text_secondary))
            .child(field)
    };
    // The pin shortcut, while the placeholder shows. Ctrl+P does nothing in the settings.
    if !s.show_settings && s.query.text.is_empty() {
        header = header.child(
            row()
                .gap(5.0)
                .center_y()
                .child(key_cap("ctrl"))
                .child(key_cap("p"))
                .child(text("pin").font(HINT_FONT).color(t.text_secondary).pl(2.0)),
        );
    }
    // The window is transparent; the rounded card is the visible launcher.
    let mut card = col()
        .w_full()
        .h_full()
        .fill(t.card)
        .radius(radius)
        .clip_rounded(radius)
        .child(header.child(gear))
        .child(el().h(1.0).w_full().fill(t.separator));
    // The compact card is full with the settings. The error stays in state for the results view.
    if !s.show_settings
        && let Some(e) = &s.error
    {
        card = card.child(
            row().w_full().px(18.0).py(8.0).child(
                text(e.clone())
                    .font(type_::BODY)
                    .color(rune::oklch(0.72, 0.16, 25.0))
                    .single_line()
                    .truncate(),
            ),
        );
    }
    let card = card.child(if s.show_settings {
        settings_view(s)
    } else {
        results(s)
    });
    // The shadow sits on a frame around the card, so the card's clip does not cut it off.
    let (width, height) = set.card();
    // With Hyprland's `stay_focused`, a click on a different window comes here, outside the
    // window's area, and focus stays. The modal sees a click outside the card.
    let mut frame = el()
        .size(width, height)
        .radius(radius)
        .child(card)
        .modal(true, |st: &mut State| st.click_away = true);
    if let Some((y, blur, alpha)) = set.shadow_spec() {
        frame = frame.shadow(y, blur, Color::BLACK.alpha(alpha));
    }
    if let Some(caret) = block_caret(s, cx, width, height) {
        frame = frame.child(caret);
    }
    col().w_full().h_full().center().child(frame)
}

/// A blinking block at the caret. The glyph it covers is drawn again in the card colour, so a
/// mid-query caret stays readable. Rune gives the caret in window points; the frame sits centred
/// in the window. The block is in the frame, so a click on it is not "outside".
fn block_caret(s: &State, cx: &Cx<State>, card_w: f32, card_h: f32) -> Option<El> {
    if s.show_settings {
        return None;
    }
    let [caret_x, caret_y, _, caret_h] = cx.caret("query")?;
    let theme = tok::theme(true);
    let (win_w, win_h) = Settings::window();
    let style = QUERY_FONT.lh(26.0);
    let glyph = caret_glyph(&s.query).map(|ch| ch.to_string());
    let block_w = glyph
        .as_deref()
        .map_or(QUERY_FONT.size * 0.55, |label| advance(label, style).ceil());
    let mut caret = el()
        .key("caret")
        .absolute(
            caret_x - (win_w - card_w) / 2.0,
            caret_y - (win_h - card_h) / 2.0,
        )
        .size(block_w, caret_h)
        .fill(theme.text)
        .cycle(CARET_BLINK)
        .a11y_hidden();
    if let Some(label) = glyph {
        caret = caret.child(text(label).key("caret-glyph").font(style).color(theme.card));
    }
    Some(caret)
}

/// The character a block caret covers: the one after the caret.
fn caret_glyph(edit: &TextEdit) -> Option<char> {
    edit.text
        .get(edit.focus.min(edit.text.len())..)
        .and_then(|rest| rest.chars().next())
}

/// `$XDG_RUNTIME_DIR` when absolute. Otherwise `/tmp/launcher-<uid>`, not shared with other users.
/// The bool is true for that fallback, which has to be checked before the lock or socket is trusted.
fn runtime_dir(var: Option<&str>, uid: u32) -> (PathBuf, bool) {
    var.filter(|p| Path::new(p).is_absolute()).map_or_else(
        || (PathBuf::from(format!("/tmp/launcher-{uid}")), true),
        |dir| (PathBuf::from(dir), false),
    )
}

/// Owner of `path`, following a symlink, or `u32::MAX` when it cannot be statted.
/// `/proc/self` is a root-owned symlink; its target is this process.
fn path_owner(path: &Path) -> u32 {
    fs::metadata(path).map_or(u32::MAX, |meta| meta.uid())
}

fn current_uid() -> u32 {
    path_owner(Path::new("/proc/self"))
}

fn instance_lock_path(dir: &Path) -> PathBuf {
    dir.join(format!("{APP_ID}.lock"))
}

fn instance_sock_path(dir: &Path) -> PathBuf {
    dir.join(format!("{APP_ID}.sock"))
}

/// Exclusive lock so only one launcher runs. Hold the file until exit.
/// `Err` = another instance holds it. `Ok(None)` = could not open the lock file; still run.
/// ponytail: open IO fail continues unlocked — better a second window than a bricked launch.
fn instance_lock(path: &Path) -> Result<Option<fs::File>, ()> {
    let Ok(file) = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)
    else {
        return Ok(None);
    };
    if file.try_lock().is_err() {
        return Err(());
    }
    Ok(Some(file))
}

/// `dir` exists, is not a symlink, is owned by `uid`, and is mode 0700. Creates it when absent.
fn ensure_private_dir(dir: &Path, uid: u32) -> bool {
    if fs::symlink_metadata(dir).is_err() {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        builder.mode(0o700);
        let _: io::Result<()> = builder.create(dir);
    }
    let Ok(meta) = fs::symlink_metadata(dir) else {
        return false;
    };
    let ty = meta.file_type();
    if ty.is_symlink() || !ty.is_dir() || meta.uid() != uid {
        return false;
    }
    if meta.mode() & 0o077 != 0 {
        return chmod_private(dir);
    }
    true
}

fn chmod_private(dir: &Path) -> bool {
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).is_ok()
}

/// Absent, or a non-symlink of `kind` owned by `uid`. Anything else is not safe to lock or nudge.
fn trusted_path(path: &Path, uid: u32, kind: impl Fn(&fs::FileType) -> bool) -> bool {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => true,
        Ok(meta) => meta.uid() == uid && kind(&meta.file_type()),
        Err(_) => false,
    }
}

/// This process's instance. `Nudge` only when a lock we trust is already held.
enum Claim {
    Run {
        lock: Option<fs::File>,
        sock: Option<PathBuf>,
    },
    Nudge(PathBuf),
}

/// Take the single-instance lock. An untrusted fallback dir, lock, or socket opens a window
/// and does not forward the activation token.
fn claim_instance(dir: &Path, uid: u32, private: bool) -> Claim {
    let lock_path = instance_lock_path(dir);
    let sock_path = instance_sock_path(dir);
    if private
        && (!ensure_private_dir(dir, uid)
            || !trusted_path(&lock_path, uid, fs::FileType::is_file)
            || !trusted_path(&sock_path, uid, fs::FileType::is_socket))
    {
        return Claim::Run {
            lock: None,
            sock: None,
        };
    }
    match instance_lock(&lock_path) {
        Ok(lock) => Claim::Run {
            lock,
            sock: Some(sock_path),
        },
        Err(()) => Claim::Nudge(sock_path),
    }
}

/// Compositor focus token from the second process's environment.
fn activation_token(xdg: Option<String>, startup: Option<String>) -> String {
    xdg.filter(|t| !t.is_empty())
        .or_else(|| startup.filter(|t| !t.is_empty()))
        .unwrap_or_default()
}

/// Bind the nudge socket. Removes a stale path left after a crash (we hold the instance lock).
fn bind_nudge_socket(path: &Path) -> Option<UnixDatagram> {
    let _: io::Result<()> = fs::remove_file(path);
    UnixDatagram::bind(path).ok()
}

/// Tell the running instance to raise, forwarding an activation token when the compositor gave one.
fn send_nudge(path: &Path, token: &str) -> bool {
    let Ok(sock) = UnixDatagram::unbound() else {
        return false;
    };
    sock.send_to(token.as_bytes(), path).is_ok()
}

/// Recv nudges until the socket errors out; each payload is an activation token (may be empty).
fn serve_nudges(sock: UnixDatagram, mut on_nudge: impl FnMut(String) + Send + 'static) {
    thread::spawn(move || {
        let mut buf = [0_u8; 4096];
        while let Ok(n) = sock.recv(&mut buf) {
            let bytes = buf.get(..n).unwrap_or(&[]);
            on_nudge(String::from_utf8_lossy(bytes).into_owned());
        }
    });
}

/// Only a real window runs this, so coverage leaves it out. The tests cover what it calls.
#[cfg_attr(coverage_nightly, coverage(off))]
fn main() {
    let xdg = env::var("XDG_RUNTIME_DIR").ok();
    let uid = current_uid();
    let (dir, private) = runtime_dir(xdg.as_deref(), uid);
    let (_lock, sock) = match claim_instance(&dir, uid, private) {
        Claim::Nudge(path) => {
            let token = activation_token(
                env::var("XDG_ACTIVATION_TOKEN").ok(),
                env::var("DESKTOP_STARTUP_ID").ok(),
            );
            let _: bool = send_nudge(&path, &token);
            return;
        }
        Claim::Run { lock, sock } => (lock, sock),
    };
    // The Wayland app_id / X11 class, so compositor window rules can match it.
    rune::set_app_id(APP_ID);
    rune::run_windows(move |_| {
        let app = App::new(AppSpec {
            // Icons decode on the cache's worker; each finished one wakes the app to repaint.
            // They rasterize at the largest size, so a size change needs no new decode.
            setup: Some(|s: &mut State, waker| {
                let cache = ImageCache::new(waker);
                s.icons = s
                    .apps
                    .iter()
                    .map(|a| cache.app_icon(&a.path, 48.0))
                    .collect();
                s.cache = Some(cache);
            }),
            // The modal's dismiss cannot close the window; this hook can.
            poll: Some(|s: &mut State, _, tx| {
                if s.click_away {
                    tx.close_window();
                }
                false
            }),
            ..AppSpec::new(
                "Launcher",
                Settings::window(),
                || State::load(Files::xdg(), load_apps(&app_dirs())),
                view,
            )
        })
        .on_focus(on_focus);
        let handle = app.handle();
        if let Some(path) = sock
            && let Some(listener) = bind_nudge_socket(&path)
        {
            serve_nudges(listener, move |token| {
                if token.is_empty() {
                    handle.focus();
                } else {
                    handle.activate(token);
                }
            });
        }
        let opts = WindowOptions {
            borderless: true,
            transparent: true,
            always_on_top: true,
            resizable: false,
            visible: true,
            ..Default::default()
        };
        vec![
            WindowSpec::from_app("launcher", app)
                .configured_options(opts.macos(rune::MacWindowOptions::default())),
        ]
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use rune::{Env, Mods, Runtime};
    use std::os::unix::fs::symlink;
    use std::{process, sync::Arc, sync::mpsc, time::Duration};

    /// No test may write the user's real files. Tests that do not save use this folder, which
    /// cannot exist, so a stray write fails quietly.
    const NO_FILES: &str = "/nonexistent/launcher-tests";

    /// An empty folder of its own for one test.
    fn temp(name: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("launcher-test-{name}-{}", process::id()));
        let _: io::Result<()> = fs::remove_dir_all(&dir);
        dir
    }

    /// Apps named `names`, each with a command that cannot start, and its files in `dir`.
    fn state(dir: &Path, names: &[&str], pinned: &[&str], settings: Settings) -> State {
        let app = |n: &&str| Entry {
            name: (*n).to_owned(),
            id: format!("{n}.desktop"),
            exec: vec![format!("/nonexistent/{n}")],
            terminal: false,
            path: format!("{n}.desktop").into(),
        };
        let mut s = State::load(Files::in_dirs(dir, dir), names.iter().map(app).collect());
        s.pinned = pinned.iter().map(|p| format!("{p}.desktop")).collect();
        s.settings = settings;
        s
    }

    /// The launcher in a headless runtime, after its first frame.
    fn launcher(s: State) -> Runtime<State> {
        let mut rt = Runtime::new(s, view, Settings::window(), Env::new(true, 1.0));
        rt.frame(0.0);
        rt
    }

    /// Press `key` in the search field, which has the focus.
    fn press(rt: &mut Runtime<State>, key: Key) {
        rt.key(KeyEvent {
            key,
            mods: Mods::default(),
        });
        rt.frame(0.0);
    }

    /// Click the element keyed `key`, then let the motion settle: a view that is still fading
    /// in takes no clicks.
    fn click(rt: &mut Runtime<State>, key: &str) {
        let (x, y) = rt.find(key).expect(key);
        rt.pointer_down(x, y);
        rt.pointer_up(x, y);
        for _ in 0_u8..60 {
            rt.frame(1.0 / 60.0);
        }
    }

    /// Read a value from the runtime's state.
    fn read<T>(rt: &mut Runtime<State>, f: impl FnOnce(&State) -> T) -> T {
        let mut out = None;
        rt.update(|s, _| out = Some(f(s)));
        out.expect("update runs its closure")
    }

    #[test]
    fn empty_query_shows_pins_then_the_most_launched() {
        let mut s = state(
            Path::new(NO_FILES),
            &["a", "b", "c", "d", "e"],
            &[],
            Settings::default(),
        );
        s.usage = [("d.desktop".to_owned(), 5), ("b.desktop".to_owned(), 2)].into();
        assert_eq!(
            matches(&s),
            [3, 1, 0, 2, 4],
            "d, b, then the rest alphabetically"
        );
        s.pinned = vec!["e.desktop".into(), "missing.desktop".into()];
        assert_eq!(
            matches(&s),
            [4, 3, 1, 0, 2],
            "the pin first; an uninstalled pin is skipped"
        );
        s.pinned = ["e", "c", "a", "b"].map(|n| format!("{n}.desktop")).into();
        assert_eq!(
            matches(&s),
            [4, 2, 0, 1, 3],
            "pins in pin order, then the rest"
        );
        assert_eq!(pin_count(&s, &matches(&s)), 4);
        s.query = TextEdit::new("e");
        assert_eq!(matches(&s), [4], "a search covers every app");
        assert_eq!(
            pin_count(&s, &matches(&s)),
            0,
            "a search does not put pins first"
        );
    }

    #[test]
    fn parses_desktop_entry() {
        let e = parse("# a comment\n[Desktop Entry]\nType=Application\nName=Foo\nExec=foo --x %U\nTerminal=true\nno equals sign\n[Desktop Action n]\nName=Bar", "/a/foo.desktop".into()).unwrap();
        assert_eq!(
            (e.name.as_str(), e.id.as_str(), e.terminal),
            ("Foo", "foo.desktop", true)
        );
        assert_eq!(e.exec, ["foo", "--x"]);
        assert!(
            parse(
                "[Desktop Entry]\nName=A\nExec=a\nNoDisplay=true",
                "x".into()
            )
            .is_none()
        );
        assert!(
            parse("[Desktop Entry]\nName=A\nExec=%U", "x".into()).is_none(),
            "no command left"
        );
        let spaced = parse(
            "[Desktop Entry]\nName = Foo\nExec = foo\nTerminal = true ",
            "x".into(),
        )
        .unwrap();
        assert_eq!(
            (spaced.name.as_str(), spaced.terminal),
            ("Foo", true),
            "spaces around = are ignored"
        );
        assert!(
            parse(
                "[Desktop Entry]\nName=A\nExec=a\nNoDisplay = true",
                "x".into()
            )
            .is_none()
        );
        assert!(
            parse("[Desktop Entry]\nType=Link\nName=A\nExec=a", "x".into()).is_none(),
            "only applications"
        );
        assert!(
            parse("[Desktop Entry]\nExec=a", "x".into()).is_none(),
            "no name"
        );
        assert!(
            parse("[Desktop Entry]\nName=A", "x".into()).is_none(),
            "no command"
        );
        assert!(
            parse("[Desktop Entry]\nName=A\nExec=a", "/".into()).is_none(),
            "no file name for the id"
        );
    }

    #[test]
    fn terminal_apps_run_inside_the_terminal() {
        let mut app = Entry {
            name: "Top".into(),
            id: "top.desktop".into(),
            exec: vec!["top".into()],
            terminal: true,
            path: "x".into(),
        };
        let term = || vec!["ghostty".to_owned(), "-e".to_owned()];
        assert_eq!(argv(&app, term()), ["ghostty", "-e", "top"]);
        app.terminal = false;
        assert_eq!(argv(&app, term()), ["top"]);
    }

    #[test]
    fn the_terminal_comes_from_the_variable() {
        assert_eq!(terminal(None), ["xdg-terminal-exec", "-e"]);
        assert_eq!(
            terminal(Some("  ".into())),
            ["xdg-terminal-exec", "-e"],
            "blank counts as unset"
        );
        assert_eq!(
            terminal(Some("foot --app-id x".into())),
            ["foot", "--app-id", "x", "-e"]
        );
    }

    #[test]
    fn a_failed_start_keeps_the_reason_and_counts_nothing() {
        let mut s = state(
            Path::new(NO_FILES),
            &["x", "empty"],
            &[],
            Settings::default(),
        );
        assert!(!start(&mut s, 0));
        assert!(
            s.error
                .as_deref()
                .is_some_and(|e| e.starts_with("Could not start /nonexistent/x"))
        );
        assert!(s.usage.is_empty());
        assert!(!start(&mut s, 2), "no such app");
        for app in &mut s.apps {
            app.exec.clear();
        }
        assert!(!start(&mut s, 1), "nothing to run");
    }

    #[test]
    fn splits_exec_per_spec() {
        let args = |v: &str| exec_args(v, "Foo", Path::new("/a/foo.desktop"));
        assert_eq!(args("printf 100%%"), ["printf", "100%"]);
        assert_eq!(
            args("foo --url=%u %F"),
            ["foo", "--url="],
            "codes in a word go; a lone code drops the argument"
        );
        assert_eq!(
            args(r#"sh -c "a  b""#),
            ["sh", "-c", "a  b"],
            "quotes keep spaces"
        );
        assert_eq!(
            args(r#""/opt/my app/run" "say \"hi\"" """#),
            ["/opt/my app/run", "say \"hi\"", ""]
        );
        assert_eq!(args(r#""a\sb" %c %k"#), ["a b", "Foo", "/a/foo.desktop"]);
        assert_eq!(
            args(r#"echo "\\\\$x""#),
            ["echo", r"\$x"],
            "four backslashes in the file are one in a quoted argument"
        );
        assert_eq!(
            args(r#"a\tb "c\nd\re""#),
            ["a", "b", "c\nd\re"],
            "string escapes; outside quotes a tab splits"
        );
        assert_eq!(
            args(r#"a\x "b\"#),
            [r"a\x", "b"],
            "unknown and trailing escapes"
        );
        assert_eq!(args("  a   b "), ["a", "b"], "a run of spaces splits once");
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
        assert_eq!(
            row_step(&rows(0, 3, 1), 2, 1),
            0,
            "the list steps one app and wraps"
        );
        assert_eq!(row_step(&[], 3, 1), 0, "no rows");
    }

    #[test]
    fn pinning_keeps_the_selection_on_the_app() {
        let mut s = state(
            Path::new(NO_FILES),
            &["a", "b", "c"],
            &[],
            Settings::default(),
        );
        s.picked = 2;
        assert!(flip_pin(&mut s));
        assert_eq!(
            (matches(&s), s.picked),
            (vec![2, 0, 1], 0),
            "c moves to the top, and the selection goes with it"
        );
        assert!(flip_pin(&mut s));
        assert_eq!(
            (matches(&s), s.picked),
            (vec![0, 1, 2], 2),
            "unpinned, c goes back, still picked"
        );
        s.query = TextEdit::new("zz");
        assert!(!flip_pin(&mut s), "nothing picked");
    }

    #[test]
    fn the_highlight_sits_on_the_picked_app() {
        let names = ["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"];
        for grid in [false, true] {
            // Pins a and b, then 8 apps: in the grid's 6 columns that is [a b] [c..h] [i j].
            let s = state(
                Path::new(NO_FILES),
                &names,
                &["a", "b"],
                Settings {
                    grid,
                    tall: true,
                    ..Settings::default()
                },
            );
            let mut rt = launcher(s);
            for picked in [0, 1, 2, 5, 8, 9, 0] {
                rt.update(|st, _| st.picked = picked);
                for _ in 0_u8..120 {
                    rt.frame(1.0 / 60.0);
                }
                let hl = rt.presented("highlight").expect("highlight").0;
                let app = rt.presented(&format!("app-{picked}")).expect("app").0;
                let off = hl
                    .iter()
                    .zip(app)
                    .map(|(h, a)| (h - a).abs())
                    .fold(0.0, f32::max);
                assert!(
                    off < 0.5,
                    "grid {grid}, app {picked}: highlight {hl:?}, app {app:?}"
                );
            }
        }
    }

    #[test]
    fn keys_move_pin_and_open_settings() {
        let dir = temp("keys");
        let names = ["a", "b", "c", "d", "e", "f", "g", "h"];
        // The grid has 6 columns: [a..f] [g h].
        let grid = Settings {
            grid: true,
            ..Settings::default()
        };
        let mut rt = launcher(state(&dir, &names, &[], grid));
        let picked_after = |runtime: &mut Runtime<State>, key: Key| {
            press(runtime, key);
            read(runtime, |s| s.picked)
        };
        assert_eq!(picked_after(&mut rt, Key::Right), 1);
        assert_eq!(picked_after(&mut rt, Key::Left), 0);
        assert_eq!(picked_after(&mut rt, Key::Left), 7, "Left wraps to the end");
        assert_eq!(
            picked_after(&mut rt, Key::Down),
            1,
            "Down wraps, same column"
        );
        assert_eq!(picked_after(&mut rt, Key::Up), 7);

        // Ctrl+P pins h, which moves to the front, and saves the pin.
        press(&mut rt, Key::Command('p'));
        assert_eq!(
            read(&mut rt, |s| (s.pinned.clone(), s.picked)),
            (vec!["h.desktop".to_owned()], 0)
        );
        assert_eq!(
            fs::read_to_string(dir.join("pinned")).ok().as_deref(),
            Some("h.desktop\n")
        );

        // Ctrl+, opens the settings, which take no list keys and no Ctrl+P.
        press(&mut rt, Key::Command(','));
        assert!(read(&mut rt, |s| s.show_settings));
        assert!(rt.find("settings").is_some());
        press(&mut rt, Key::Down);
        press(&mut rt, Key::Command('p'));
        assert_eq!(read(&mut rt, |s| (s.picked, s.pinned.len())), (0, 1));
        press(&mut rt, Key::Escape);
        assert!(
            !read(&mut rt, |s| s.show_settings),
            "Escape closes the settings"
        );

        // A search with no result: the keys have nothing to move, and no highlight shows.
        press(&mut rt, Key::Text("zz".into()));
        assert_eq!(picked_after(&mut rt, Key::Down), 0);
        assert!(rt.find("highlight").is_none());
        press(&mut rt, Key::Command('p'));
        assert_eq!(read(&mut rt, |s| s.pinned.len()), 1, "no app to pin");
        // Escape asks to close the window; the runtime keeps that request to itself.
        press(&mut rt, Key::Escape);

        // In the list, Left and Right belong to the text caret.
        let mut list = launcher(state(&dir, &names, &[], Settings::default()));
        assert_eq!(picked_after(&mut list, Key::Right), 0);
        assert_eq!(picked_after(&mut list, Key::Down), 1);
        let _: io::Result<()> = fs::remove_dir_all(&dir);
    }

    #[test]
    fn settings_buttons_change_and_save() {
        let dir = temp("settings");
        let mut rt = launcher(state(&dir, &["a"], &["a"], Settings::default()));
        press(&mut rt, Key::Text("a".into()));
        click(&mut rt, "gear");
        assert!(read(&mut rt, |s| s.show_settings));
        assert!(
            rt.find("credit").is_some(),
            "settings shows the GitHub credit"
        );
        assert_eq!(
            read(&mut rt, |s| s.query.text.clone()),
            "a",
            "opening settings keeps the query"
        );
        press(&mut rt, Key::Text("x".into()));
        assert_eq!(
            read(&mut rt, |s| s.query.text.clone()),
            "a",
            "typing in settings does not edit the query"
        );
        // Each button once; the last in each row stays.
        let clicks = [
            ("Layout", ["List", "Grid"].as_slice()),
            ("Icon size", &["Small", "Medium", "Large"]),
            ("Name size", &["Small", "Large", "Medium"]),
            ("Names", &["Show", "Hide"]),
            ("Height", &["Compact", "Tall"]),
            ("Corners", &["Sharp", "Rounded", "Soft", "Round"]),
            ("Shadow", &["None", "Strong", "Soft"]),
        ];
        for (label, names) in clicks {
            for name in names {
                click(&mut rt, &format!("set-{label}-{name}"));
            }
        }
        click(&mut rt, &format!("hl-{}", swatch(2).0));
        let want = Settings {
            grid: true,
            icon: 48,
            text: 15,
            names: false,
            tall: true,
            corners: Corners::Round,
            shadow: 1,
            highlight: 2,
        };
        assert_eq!(read(&mut rt, |s| s.settings), want);
        assert_eq!(Settings::load(&dir.join("settings")), want, "saved");
        assert!(
            read(&mut rt, |s| s.query.focused),
            "the field keeps the keys"
        );

        // Enter in the settings starts nothing.
        press(&mut rt, Key::Enter);
        assert!(read(&mut rt, |s| s.error.is_none()));
        // The gear closes the settings, and the list shows in the new layout.
        click(&mut rt, "gear");
        assert!(rt.find("app-0").is_some());
        let _: io::Result<()> = fs::remove_dir_all(&dir);
    }

    #[test]
    fn enter_and_click_start_apps() {
        let dir = temp("launch");
        let mut s = state(&dir, &["a", "b"], &[], Settings::default());
        // `true` starts and exits at once.
        let b = s.apps.iter_mut().find(|a| a.name == "b").expect("app b");
        b.exec = vec!["true".into()];
        let mut rt = launcher(s);

        // Enter starts the picked app; a failed start shows why and keeps the window.
        press(&mut rt, Key::Enter);
        assert!(read(&mut rt, |st| st.error.as_deref().is_some_and(
            |e| e.starts_with("Could not start /nonexistent/a")
        )));
        // Typing clears the error, and Enter with no result does nothing.
        press(&mut rt, Key::Text("zz".into()));
        assert!(read(&mut rt, |st| st.error.is_none()));
        press(&mut rt, Key::Enter);
        assert!(read(&mut rt, |st| st.error.is_none()));
        press(&mut rt, Key::Backspace);
        press(&mut rt, Key::Backspace);

        // A click starts that app and counts the launch.
        click(&mut rt, "app-1");
        assert_eq!(
            read(&mut rt, |st| (
                st.picked,
                st.usage.get("b.desktop").copied()
            )),
            (1, Some(1))
        );
        assert_eq!(
            load_usage(&dir.join("usage")),
            Usage::from([("b.desktop".to_owned(), 1)])
        );
        let _: io::Result<()> = fs::remove_dir_all(&dir);
    }

    #[test]
    fn saved_files_load_at_start() {
        let dir = temp("files");
        let files = || Files::in_dirs(&dir.join("config"), &dir.join("state"));
        let fresh = State::load(files(), vec![]);
        assert_eq!(fresh.settings, Settings::default(), "nothing saved yet");
        assert!(fresh.usage.is_empty() && fresh.pinned.is_empty());
        assert!(fresh.query.focused);

        let set = Settings {
            shadow: 2,
            ..Settings::default()
        };
        set.save(&fresh.files.settings);
        write_file(
            fresh.files.usage.clone(),
            "3\ta.desktop\nbad line\nx\tb.desktop\n".to_owned(),
        );
        write_file(fresh.files.pinned, "b.desktop\n".to_owned());
        let again = State::load(files(), vec![]);
        assert_eq!(again.settings, set);
        assert_eq!(
            again.usage,
            Usage::from([("a.desktop".to_owned(), 3)]),
            "bad lines are skipped"
        );
        assert_eq!(again.pinned, ["b.desktop"]);

        let xdg = Files::xdg();
        assert!(xdg.settings.ends_with("sh.bzn.launcher/settings"));
        assert!(xdg.pinned.ends_with("sh.bzn.launcher/pinned"));
        assert!(xdg.usage.ends_with("sh.bzn.launcher/usage"));
        let _: io::Result<()> = fs::remove_dir_all(&dir);
    }

    #[test]
    fn runtime_paths_use_xdg_runtime_or_a_per_user_dir() {
        let (run, xdg) = runtime_dir(Some("/run/user/1000"), 1000);
        assert!(!xdg, "an absolute runtime dir stays as it is");
        assert_eq!(
            instance_lock_path(&run),
            PathBuf::from("/run/user/1000/sh.bzn.launcher.lock")
        );
        assert_eq!(
            instance_sock_path(&run),
            PathBuf::from("/run/user/1000/sh.bzn.launcher.sock")
        );
        let (tmp, fallback) = runtime_dir(Some("relative"), 1000);
        assert!(fallback, "relative counts as unset");
        assert_eq!(tmp, PathBuf::from("/tmp/launcher-1000"));
        assert_eq!(runtime_dir(None, 5).0, PathBuf::from("/tmp/launcher-5"));
    }

    #[test]
    fn instance_lock_blocks_a_second_handle_until_drop() {
        let dir = temp("lock");
        let _: io::Result<()> = fs::create_dir_all(&dir);
        let path = instance_lock_path(&dir);
        let first = instance_lock(&path)
            .expect("first should run")
            .expect("holds the lock");
        assert!(
            instance_lock(&path).is_err(),
            "second handle sees WouldBlock"
        );
        drop(first);
        let again = instance_lock(&path)
            .expect("after drop should run")
            .expect("holds the lock again");
        drop(again);
        let _: io::Result<()> = fs::remove_dir_all(&dir);
    }

    #[test]
    fn instance_lock_allows_start_when_path_unusable() {
        assert!(
            matches!(
                instance_lock(Path::new("/nonexistent/launcher-lock-test/x.lock")),
                Ok(None)
            ),
            "missing parent must not brick launch"
        );
    }

    #[test]
    fn activation_token_prefers_xdg_then_startup_id() {
        assert_eq!(
            activation_token(Some("way".into()), Some("x11".into())),
            "way"
        );
        assert_eq!(
            activation_token(Some(String::new()), Some("x11".into())),
            "x11"
        );
        assert_eq!(activation_token(None, None), "");
    }

    #[test]
    fn nudge_socket_delivers_the_token() {
        let dir = temp("nudge");
        let _: io::Result<()> = fs::create_dir_all(&dir);
        let path = instance_sock_path(&dir);
        let listener = bind_nudge_socket(&path).expect("bind");
        let (tx, rx) = mpsc::channel();
        serve_nudges(listener, move |token| {
            let _: Result<(), _> = tx.send(token);
        });
        assert!(send_nudge(&path, "tok-1"), "send reaches the listener");
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), "tok-1");
        assert!(send_nudge(&path, ""), "empty token is a focus-only nudge");
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), "");
        assert!(
            !send_nudge(Path::new("/nonexistent/launcher-nudge.sock"), "x"),
            "missing socket fails quietly"
        );
        assert!(
            bind_nudge_socket(Path::new("/nonexistent/launcher-nudge.sock")).is_none(),
            "bind fails without a parent dir"
        );
        let _: io::Result<()> = fs::remove_file(&path);
        let _: io::Result<()> = fs::remove_dir_all(&dir);
    }

    #[test]
    fn path_owner_is_this_user_or_max_when_missing() {
        let dir = temp("uid");
        let _: io::Result<()> = fs::create_dir_all(&dir);
        let path = dir.join("f");
        fs::write(&path, b"").unwrap();
        assert_eq!(path_owner(&path), current_uid());
        assert_eq!(
            path_owner(Path::new("/nonexistent/launcher-uid")),
            u32::MAX,
            "an unreadable path is not a real uid"
        );
        let _: io::Result<()> = fs::remove_dir_all(&dir);
    }

    #[test]
    fn private_dir_is_created_0700_and_rejects_what_we_do_not_own() {
        let dir = temp("private");
        let uid = current_uid();
        let created = dir.join("new");
        assert!(ensure_private_dir(&created, uid), "creates the directory");
        assert_eq!(
            fs::symlink_metadata(&created).unwrap().mode() & 0o777,
            0o700
        );
        let open = dir.join("open");
        let _: io::Result<()> = fs::create_dir_all(&open);
        fs::set_permissions(&open, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(
            ensure_private_dir(&open, uid),
            "a directory we own is tightened"
        );
        assert_eq!(fs::symlink_metadata(&open).unwrap().mode() & 0o777, 0o700);
        fs::set_permissions(&open, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(ensure_private_dir(&open, uid), "already private");
        assert!(
            !ensure_private_dir(&open, uid.wrapping_add(1)),
            "a different owner is not ours"
        );
        let file = dir.join("file");
        fs::write(&file, b"x").unwrap();
        assert!(!ensure_private_dir(&file, uid), "a file is not a directory");
        let link = dir.join("link");
        symlink(&open, &link).unwrap();
        assert!(!ensure_private_dir(&link, uid), "a symlink is not followed");
        let blocked = file.join("child");
        assert!(
            !ensure_private_dir(&blocked, uid),
            "cannot create a directory under a file"
        );
        assert!(!chmod_private(Path::new("/nonexistent/launcher-chmod")));
        let _: io::Result<()> = fs::remove_dir_all(&dir);
    }

    #[test]
    fn trusted_path_rejects_the_wrong_owner_type_or_link() {
        let dir = temp("trust");
        let _: io::Result<()> = fs::create_dir_all(&dir);
        let uid = current_uid();
        let file = dir.join("f");
        fs::write(&file, b"x").unwrap();
        assert!(trusted_path(&file, uid, fs::FileType::is_file));
        assert!(!trusted_path(&file, uid, fs::FileType::is_socket));
        assert!(
            !trusted_path(&file, uid.wrapping_add(1), fs::FileType::is_file),
            "someone else's file"
        );
        assert!(trusted_path(
            &dir.join("missing"),
            uid,
            fs::FileType::is_file
        ));
        let link = dir.join("link");
        symlink(&file, &link).unwrap();
        assert!(
            !trusted_path(&link, uid, fs::FileType::is_file),
            "a symlink is not the file"
        );
        let sock = dir.join("s.sock");
        let _listener = UnixDatagram::bind(&sock).unwrap();
        assert!(trusted_path(&sock, uid, fs::FileType::is_socket));
        assert!(!trusted_path(&sock, uid, fs::FileType::is_file));
        let hidden = dir.join("hidden");
        let _: io::Result<()> = fs::create_dir_all(&hidden);
        let inside = hidden.join("f");
        fs::write(&inside, b"x").unwrap();
        fs::set_permissions(&hidden, fs::Permissions::from_mode(0o000)).unwrap();
        assert!(
            !trusted_path(&inside, uid, fs::FileType::is_file),
            "a path we cannot stat is not trusted"
        );
        fs::set_permissions(&hidden, fs::Permissions::from_mode(0o700)).unwrap();
        let _: io::Result<()> = fs::remove_dir_all(&dir);
    }

    #[test]
    fn untrusted_fallback_opens_a_window_instead_of_nudging() {
        let dir = temp("claim");
        let _: io::Result<()> = fs::create_dir_all(&dir);
        let uid = current_uid();
        let held = |claim: &Claim| {
            matches!(
                claim,
                Claim::Run {
                    lock: Some(_),
                    sock: Some(_)
                }
            )
        };
        let refused = |claim: &Claim| {
            matches!(
                claim,
                Claim::Run {
                    lock: None,
                    sock: None
                }
            )
        };

        // The runtime dir is not checked: a loose directory still locks and nudges.
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o777)).unwrap();
        let first = claim_instance(&dir, uid, false);
        assert!(held(&first), "the xdg path locks as before");
        assert!(
            matches!(claim_instance(&dir, uid, false), Claim::Nudge(_)),
            "a second start nudges"
        );
        drop(first);

        // The fallback refuses a symlink, a foreign owner, and a lock or socket we do not own.
        let link = dir.join("link");
        symlink(&dir, &link).unwrap();
        assert!(refused(&claim_instance(&link, uid, true)), "symlink dir");
        assert!(
            refused(&claim_instance(&dir, uid.wrapping_add(1), true)),
            "dir owned by someone else"
        );
        let home = dir.join("home");
        let ours = claim_instance(&home, uid, true);
        assert!(held(&ours), "a private dir we create is held");
        assert_eq!(fs::symlink_metadata(&home).unwrap().mode() & 0o777, 0o700);
        let sock = instance_sock_path(&home);
        assert!(
            matches!(claim_instance(&home, uid, true), Claim::Nudge(path) if path == sock),
            "a lock we own forwards to our socket"
        );
        drop(ours);

        let lock = instance_lock_path(&home);
        let _: io::Result<()> = fs::remove_file(&lock);
        symlink(&dir, &lock).unwrap();
        assert!(refused(&claim_instance(&home, uid, true)), "symlink lock");
        let _: io::Result<()> = fs::remove_file(&lock);
        fs::write(instance_sock_path(&home), b"nope").unwrap();
        assert!(
            refused(&claim_instance(&home, uid, true)),
            "a socket that is not ours"
        );
        let _: io::Result<()> = fs::remove_dir_all(&dir);
    }

    #[test]
    fn apps_load_from_each_dir_and_the_first_wins() {
        let dir = temp("apps");
        let (home, system) = (dir.join("home"), dir.join("system"));
        let entry = |name: &str| format!("[Desktop Entry]\nName={name}\nExec=x");
        write_file(home.join("applications/b.desktop"), entry("Bee"));
        write_file(home.join("applications/notes.txt"), entry("Notes"));
        write_file(
            system.join("applications/b.desktop"),
            entry("Hidden by home"),
        );
        write_file(system.join("applications/a.desktop"), entry("ant"));
        write_file(
            system.join("applications/off.desktop"),
            format!("{}\nHidden=true", entry("Off")),
        );
        // A folder named like an entry cannot be read, so it is skipped.
        let _: io::Result<()> = fs::create_dir_all(system.join("applications/folder.desktop"));
        let apps = load_apps(&[home, system, dir.join("missing")]);
        let names: Vec<&str> = apps.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["ant", "Bee"], "sorted without case");
        assert!(
            app_dirs().len() >= 2,
            "the data home and at least one data dir"
        );
        let _: io::Result<()> = fs::remove_dir_all(&dir);
    }

    #[test]
    fn decoded_icons_replace_the_initial() {
        let dir = temp("icons");
        let svg = dir.join("icon.svg");
        write_file(
            svg.clone(),
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16"><rect width="16" height="16" fill="red"/></svg>"#.to_owned(),
        );
        let with_icon = dir.join("a.desktop");
        write_file(
            with_icon.clone(),
            format!("[Desktop Entry]\nName=a\nExec=a\nIcon={}", svg.display()),
        );
        let cache = ImageCache::new(Arc::new(|| {}));
        let a_icon = cache.app_icon(&with_icon, 48.0);
        let b_icon = cache.app_icon(dir.join("missing.desktop"), 48.0);
        assert!(a_icon.wait().is_ok(), "the SVG decodes");
        assert!(b_icon.wait().is_err(), "no file, no icon");

        let mut s = state(Path::new(NO_FILES), &["a", "b"], &[], Settings::default());
        s.icons = vec![a_icon, b_icon];
        let rt = launcher(s);
        let shown = |t: &str| rt.elements().iter().any(|e| e.text.as_deref() == Some(t));
        assert!(!shown("A"), "a shows its icon, not its initial");
        assert!(shown("B"), "b has no icon, so it shows its initial");
        let _: io::Result<()> = fs::remove_dir_all(&dir);
    }

    #[test]
    fn xdg_dirs_fall_back_to_home() {
        let home = PathBuf::from(env::var("HOME").unwrap_or_default());
        assert_eq!(
            xdg_dir("LAUNCHER_TEST_UNSET_VARIABLE", ".x"),
            home.join(".x")
        );
        // Cargo gives every test run this variable, as an absolute path.
        assert_eq!(
            xdg_dir("CARGO_MANIFEST_DIR", ".y"),
            PathBuf::from(env!("CARGO_MANIFEST_DIR")),
            "an absolute value is used as is"
        );
    }

    #[test]
    fn pins_drop_blanks_and_repeats() {
        assert_eq!(
            parse_pinned("a.desktop\n\n b.desktop\na.desktop\n"),
            ["a.desktop", "b.desktop"]
        );
    }

    #[test]
    fn data_dirs_ignore_empty_and_relative() {
        let default: Vec<PathBuf> = vec!["/usr/local/share".into(), "/usr/share".into()];
        assert_eq!(data_dirs(None), default);
        assert_eq!(data_dirs(Some(String::new())), default);
        assert_eq!(
            data_dirs(Some("rel::/nix/share".into())),
            [PathBuf::from("/nix/share")]
        );
    }

    #[test]
    fn grid_columns_fit_the_card() {
        for icon in [24, 32, 48] {
            let s = Settings {
                grid: true,
                icon,
                ..Settings::default()
            };
            let n = to_f32(s.cols());
            assert!(
                (n - 1.0).mul_add(GAP, n * s.cell()) <= 2.0_f32.mul_add(-LIST_PAD, s.card().0),
                "icon {icon}"
            );
        }
        assert_eq!(Settings::default().cols(), 1);
    }

    #[test]
    fn settings_round_trip() {
        for corners in Corners::ALL {
            let s = Settings {
                grid: true,
                icon: 48,
                text: 17,
                names: false,
                tall: true,
                corners,
                shadow: 2,
                highlight: 3,
            };
            assert_eq!(Settings::parse(&s.to_text()), s);
        }
        assert_eq!(
            Settings::parse("junk\nicon=abc\nhighlight=chartreuse\ncorners=wobbly\nother=1"),
            Settings::default()
        );
    }

    #[test]
    fn highlight_text_is_readable() {
        for i in 0..SWATCHES {
            let (name, c) = swatch(i);
            assert!(
                contrast(ink(c), c) >= 4.5,
                "{name}: {}",
                contrast(ink(c), c)
            );
        }
        let t = tok::theme(true);
        assert_eq!(ink(Color::WHITE), t.bg, "white keeps the dark theme ink");
        assert_eq!(ink(Color::BLACK), t.text, "black takes the light ink");
        assert_eq!(swatch(SWATCHES).0, "white", "past the last colour is white");
    }

    #[test]
    fn focus_loss_closes_only_after_a_gain() {
        let mut s = state(Path::new(NO_FILES), &[], &[], Settings::default());
        assert!(
            !closes_on_focus(&mut s, false),
            "a loss before the first gain keeps it open"
        );
        assert!(!closes_on_focus(&mut s, true), "a gain keeps it open");
        assert!(closes_on_focus(&mut s, false), "a click away closes it");
        // The hook itself: a gain, then a loss, which asks to close.
        let mut hooked = state(Path::new(NO_FILES), &[], &[], Settings::default());
        let mut tx = Tx::default();
        on_focus(&mut hooked, true, &mut tx);
        on_focus(&mut hooked, false, &mut tx);
        assert!(hooked.had_focus, "the hook records the gain");
    }

    #[test]
    fn a_click_outside_the_card_closes() {
        let names = ["Alpha", "Beta"];
        let mut rt = launcher(state(Path::new(NO_FILES), &names, &[], Settings::default()));
        click(&mut rt, "gear");
        assert!(read(&mut rt, |s| s.show_settings), "a click on the card");
        assert!(
            !read(&mut rt, |s| s.click_away),
            "a click on the card keeps it open"
        );
        // Hyprland's `stay_focused` sends a click on a different window here, outside the area.
        rt.pointer_down(-356.0, 246.0);
        rt.pointer_up(-356.0, 246.0);
        rt.frame(0.0);
        assert!(
            read(&mut rt, |s| s.click_away),
            "a click outside the window closes it"
        );
    }

    #[test]
    fn the_pointer_shows_what_can_be_clicked() {
        let names = ["Alpha", "Beta"];
        let mut rt = launcher(state(Path::new(NO_FILES), &names, &[], Settings::default()));
        assert_eq!(cursor_on(&mut rt, "gear", 0.0), Cursor::Pointer, "the gear");
        assert_eq!(
            cursor_on(&mut rt, "app-0", 0.0),
            Cursor::Pointer,
            "an app row"
        );
        assert_eq!(
            cursor_on(&mut rt, "query", 0.0),
            Cursor::Text,
            "the search field"
        );
        rt.pointer_move(2.0, 2.0);
        rt.frame(0.0);
        assert_eq!(rt.cursor(), Cursor::Default, "outside the card");
        click(&mut rt, "gear");
        assert_eq!(
            cursor_on(&mut rt, "set-Layout-List", 0.0),
            Cursor::Pointer,
            "a settings button"
        );
        // The "Layout" label, left of its control.
        assert_eq!(
            cursor_on(&mut rt, "set-Layout-List", -CONTROL_W),
            Cursor::Default,
            "a settings label"
        );
        assert_eq!(
            cursor_on(&mut rt, "credit", 0.0),
            Cursor::Default,
            "the credit is not a link"
        );
        assert_eq!(
            cursor_on(&mut rt, "github", 0.0),
            Cursor::Pointer,
            "the GitHub link"
        );
        assert_eq!(cursor_on(&mut rt, "x", 0.0), Cursor::Pointer, "the X link");
    }

    #[test]
    fn the_caret_is_a_box_in_the_search_field() {
        let mut rt = launcher(state(Path::new(NO_FILES), &[], &[], Settings::default()));
        press(&mut rt, Key::Text("ab".into()));
        let (_, field_y) = rt.find("query").expect("query");
        let (_, box_y) = rt.find("caret").expect("a caret box while searching");
        assert!(
            (box_y - field_y).abs() < 1.0,
            "box at {box_y}, field at {field_y}"
        );
        assert!(
            rt.elements()
                .iter()
                .all(|e| e.key.as_deref() != Some("caret-glyph")),
            "the caret at the end covers no character"
        );
        press(&mut rt, Key::Left);
        let glyph = rt
            .elements()
            .into_iter()
            .find(|e| e.key.as_deref() == Some("caret-glyph"))
            .expect("the covered character");
        assert_eq!(glyph.text.as_deref(), Some("b"));
        assert_eq!(glyph.text_color, Some(tok::theme(true).card));
        let (caret, _) = rt.presented("caret").expect("caret");
        let (ink, _) = rt.presented("caret-glyph").expect("glyph");
        assert!(
            ink[0] >= caret[0] - 0.5 && ink[1] >= caret[1] - 0.5,
            "glyph {ink:?} sits on the block {caret:?}"
        );
        press(&mut rt, Key::Left);
        let covered = rt
            .elements()
            .into_iter()
            .find(|e| e.key.as_deref() == Some("caret-glyph"))
            .expect("the covered character");
        assert_eq!(covered.text.as_deref(), Some("a"));
        click(&mut rt, "gear");
        assert!(rt.find("caret").is_none(), "no caret in the settings");
        let on = |t: f64| CARET_BLINK.sample(t);
        assert!(on(0.2_f64) > 0.99 && on(0.7_f64) < 0.01, "on, then off");
        assert!(on(1.26_f64) > 0.99, "on again in the next period");
    }

    #[test]
    fn caret_glyph_is_the_character_after_the_caret() {
        let mut edit = TextEdit::new("ab");
        assert!(caret_glyph(&edit).is_none(), "the caret starts at the end");
        edit.set_caret(1);
        assert_eq!(caret_glyph(&edit), Some('b'));
        edit.set_caret(0);
        assert_eq!(caret_glyph(&edit), Some('a'));
        edit.text = "é".to_owned();
        edit.focus = 1;
        assert!(
            caret_glyph(&edit).is_none(),
            "a byte inside a character is not a glyph"
        );
    }

    #[test]
    fn settings_hide_the_launch_error() {
        let mut rt = launcher(state(Path::new(NO_FILES), &["a"], &[], Settings::default()));
        press(&mut rt, Key::Enter);
        let shown = |runtime: &mut Runtime<State>| {
            runtime.elements().iter().any(|e| {
                e.text
                    .as_deref()
                    .is_some_and(|t| t.starts_with("Could not start"))
            })
        };
        assert!(
            shown(&mut rt),
            "the results view shows why the launch failed"
        );
        click(&mut rt, "gear");
        assert!(read(&mut rt, |s| s.error.is_some()), "the error stays");
        assert!(!shown(&mut rt), "settings hide it");
        let (_, card_h) = Settings::default().card();
        let (_, win_h) = Settings::window();
        let bottom = f32::midpoint(win_h, card_h);
        let (_, y) = rt.find("credit").expect("credit");
        assert!(
            y + FOOTER_H / 2.0 <= bottom + 0.5,
            "footer stays inside the card"
        );
        click(&mut rt, "gear");
        assert!(shown(&mut rt), "the results view shows the error again");
    }

    /// The pointer's shape over the element keyed `key`, moved `dx` across.
    fn cursor_on(rt: &mut Runtime<State>, key: &str, dx: f32) -> Cursor {
        let (x, y) = rt.find(key).expect(key);
        rt.pointer_move(x + dx, y);
        rt.frame(0.0);
        rt.cursor()
    }

    #[test]
    fn the_credit_fits_the_compact_card() {
        let mut rt = launcher(state(Path::new(NO_FILES), &[], &[], Settings::default()));
        click(&mut rt, "gear");
        let (_, card_h) = Settings::default().card();
        let (_, win_h) = Settings::window();
        let bottom = f32::midpoint(win_h, card_h);
        let (_, y) = rt.find("credit").expect("credit");
        assert!(
            y + FOOTER_H / 2.0 <= bottom + 0.5,
            "footer bottom at {}, card bottom at {bottom}",
            y + FOOTER_H / 2.0
        );
    }
}
