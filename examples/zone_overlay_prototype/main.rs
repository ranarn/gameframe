//! PROTOTYPE — throwaway, not part of the shipped binary. Answers wayfinder
//! ticket #18 (map #14): what state model does the zone-editing overlay's
//! drag-to-draw + handle-resize interaction need, and how do "pinned"
//! (screen-edge-locked) edges restrict which handles are draggable for a
//! given preset type?
//!
//! Run with `cargo run --example zone_overlay_prototype`. See NOTES.md for
//! the question in full and, once captured, the verdict.

mod zone_editor;

use std::io::{self, BufRead, Write};
use zone_editor::{Action, AppState, EditorState, Handle, Monitor, PinnedEdges, Point, TaskbarState, Zone};

fn main() {
    // A 1080p monitor including the taskbar strip — the prototype's whole
    // point is proving a zone can be drawn/resized into that bottom strip
    // while the taskbar is auto-hidden, so the "screen" here is the full
    // monitor, not a taskbar-excluded work area.
    let mut state = AppState::new(Monitor { width: 1920, height: 1080 });

    let stdin = io::stdin();
    render(&state);
    for line in stdin.lock().lines() {
        let line = line.expect("stdin read");
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed == "q" || trimmed == "quit" {
            break;
        }
        match parse(trimmed) {
            Ok(action) => state.apply(action),
            Err(err) => state.last_message_set(&err),
        }
        render(&state);
    }
}

impl AppState {
    fn last_message_set(&mut self, msg: &str) {
        self.last_message = format!("parse error: {msg}");
    }
}

fn parse(line: &str) -> Result<Action, String> {
    let mut parts = line.split_whitespace();
    let cmd = parts.next().unwrap_or_default();
    match cmd {
        "new" => Ok(Action::New),
        "edit" => {
            let which = parts.next().ok_or("usage: edit <left|right|custom>")?;
            match which {
                "left" => Ok(Action::EditPreset {
                    pinned: PinnedEdges::LEFT_HALF,
                    zone: Zone { x: 0, y: 0, width: 960, height: 1080 },
                }),
                "right" => Ok(Action::EditPreset {
                    pinned: PinnedEdges::RIGHT_HALF,
                    zone: Zone { x: 960, y: 0, width: 960, height: 1080 },
                }),
                "custom" => Ok(Action::EditPreset {
                    pinned: PinnedEdges::NONE,
                    zone: Zone { x: 200, y: 200, width: 600, height: 500 },
                }),
                other => Err(format!("unknown preset '{other}' (want left|right|custom)")),
            }
        }
        "start" => Ok(Action::Start(parse_point(parts)?)),
        "drag" => Ok(Action::Drag(parse_point(parts)?)),
        "commit" => Ok(Action::Commit),
        "grab" => {
            let which = parts.next().ok_or("usage: grab <n|s|e|w|ne|nw|se|sw>")?;
            Ok(Action::Grab(parse_handle(which)?))
        }
        "save" => Ok(Action::Save),
        "cancel" => Ok(Action::Cancel),
        other => Err(format!("unknown command '{other}'")),
    }
}

fn parse_point<'a>(mut parts: impl Iterator<Item = &'a str>) -> Result<Point, String> {
    let x = parts.next().ok_or("usage: <cmd> x y")?.parse::<i32>().map_err(|_| "x must be an integer")?;
    let y = parts.next().ok_or("usage: <cmd> x y")?.parse::<i32>().map_err(|_| "y must be an integer")?;
    Ok(Point { x, y })
}

fn parse_handle(s: &str) -> Result<Handle, String> {
    match s {
        "n" => Ok(Handle::N),
        "s" => Ok(Handle::S),
        "e" => Ok(Handle::E),
        "w" => Ok(Handle::W),
        "ne" => Ok(Handle::Ne),
        "nw" => Ok(Handle::Nw),
        "se" => Ok(Handle::Se),
        "sw" => Ok(Handle::Sw),
        other => Err(format!("unknown handle '{other}'")),
    }
}

fn render(state: &AppState) {
    print!("\x1b[2J\x1b[H");

    println!("\x1b[1mZone overlay prototype\x1b[0m  \x1b[2m(ticket #18)\x1b[0m");
    println!(
        "\x1b[1mmonitor\x1b[0m   {}x{}",
        state.monitor.width, state.monitor.height
    );
    println!(
        "\x1b[1mtaskbar\x1b[0m   {}",
        match state.taskbar {
            TaskbarState::AlwaysVisible => "AlwaysVisible",
            TaskbarState::AutoHidden => "\x1b[2mAutoHidden\x1b[0m (forced while editing)",
        }
    );
    println!();

    match &state.editor {
        EditorState::Empty => {
            println!("\x1b[1mstate\x1b[0m     Empty — no zone drawn");
        }
        EditorState::Dragging { anchor, current } => {
            println!("\x1b[1mstate\x1b[0m     Dragging (initial corner-to-corner draw)");
            println!("  anchor  ({}, {})", anchor.x, anchor.y);
            println!("  current ({}, {})", current.x, current.y);
        }
        EditorState::Placed { zone, pinned } => {
            println!("\x1b[1mstate\x1b[0m     Placed (idle, handles active)");
            print_zone(zone);
            print_handles(*pinned);
        }
        EditorState::Resizing { pinned, handle, live, .. } => {
            println!("\x1b[1mstate\x1b[0m     Resizing via {handle:?}");
            print_zone(live);
            print_handles(*pinned);
        }
    }

    println!();
    println!("\x1b[2m{}\x1b[0m", state.last_message);
    println!();
    println!(
        "\x1b[1mnew\x1b[0m \x1b[2mfreeform\x1b[0m  \x1b[1medit\x1b[0m \x1b[2mleft|right|custom\x1b[0m  \x1b[1mstart\x1b[0m \x1b[2mx y\x1b[0m  \x1b[1mdrag\x1b[0m \x1b[2mx y\x1b[0m  \x1b[1mcommit\x1b[0m"
    );
    println!(
        "\x1b[1mgrab\x1b[0m \x1b[2mn|s|e|w|ne|nw|se|sw\x1b[0m  \x1b[1msave\x1b[0m  \x1b[1mcancel\x1b[0m  \x1b[1mq\x1b[0m\x1b[2muit\x1b[0m"
    );
    io::stdout().flush().ok();
}

fn print_zone(zone: &Zone) {
    println!(
        "  zone      x={} y={} w={} h={}  (right={} bottom={})",
        zone.x,
        zone.y,
        zone.width,
        zone.height,
        zone.x + zone.width,
        zone.y + zone.height
    );
}

fn print_handles(pinned: PinnedEdges) {
    let marks: Vec<String> = Handle::ALL
        .iter()
        .map(|h| {
            let enabled = h.enabled(pinned);
            let label = format!("{h:?}").to_lowercase();
            if enabled {
                format!("\x1b[1m{label}\x1b[0m")
            } else {
                format!("\x1b[2m{label}(locked)\x1b[0m")
            }
        })
        .collect();
    println!("  handles   {}", marks.join("  "));
}
