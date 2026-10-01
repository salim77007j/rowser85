//! xdriver — synthetic X11 input for UI testing (XTEST protocol).
//!
//! Drives the Rrowser85 window (or anything on the screen) with real
//! key/button/motion events so automated validation exercises the actual
//! event loop, exactly like a human at the keyboard.
//!
//! Usage:
//!   xdriver move <x> <y>
//!   xdriver click <x> <y>            (left)
//!   xdriver right-click <x> <y>
//!   xdriver middle-click <x> <y>
//!   xdriver double-click <x> <y>
//!   xdriver key <keysym>             (e.g. Return, Escape, F11, Left)
//!   xdriver ctrl <key>  / alt <key> / shift <key>
//!   xdriver ctrl-shift <key>
//!   xdriver type "text"
//!   xdriver scroll <dy>              (button 4/5)
//!   xdriver wait <ms>

use std::collections::HashMap;
use std::time::Duration;

use x11rb::connection::Connection;
type Keysym = u32;
use x11rb::protocol::xproto::{self, ConfigureWindowAux, ConnectionExt};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: xdriver <command> [args]  (see source header)");
        std::process::exit(2);
    }
    let (conn, screen_num) = RustConnection::connect(None)?;
    let root = conn.setup().roots[screen_num].root;
    bring_window_front(&conn, root)?;

    let map = keymap(&conn)?;

    match args[0].as_str() {
        "move" => warp(&conn, root, args[1].parse()?, args[2].parse()?),
        "click" | "right-click" | "middle-click" => {
            let x: i16 = args[1].parse()?;
            let y: i16 = args[2].parse()?;
            let button = match args[0].as_str() {
                "right-click" => 3,
                "middle-click" => 2,
                _ => 1,
            };
            warp(&conn, root, x, y);
            std::thread::sleep(Duration::from_millis(60));
            button_click(&conn, button);
            if args[0] == "click" && args.len() > 3 && args[3] == "double" {
                std::thread::sleep(Duration::from_millis(120));
                button_click(&conn, button);
            }
        }
        "double-click" => {
            let x: i16 = args[1].parse()?;
            let y: i16 = args[2].parse()?;
            warp(&conn, root, x, y);
            std::thread::sleep(Duration::from_millis(60));
            button_click(&conn, 1);
            std::thread::sleep(Duration::from_millis(80));
            button_click(&conn, 1);
        }
        "key" => key_tap(&conn, &map, &args[1])?,
        "ctrl" => modifier_tap(&conn, &map, "Control_L", &args[1])?,
        "alt" => modifier_tap(&conn, &map, "Alt_L", &args[1])?,
        "shift" => modifier_tap(&conn, &map, "Shift_L", &args[1])?,
        "ctrl-shift" => {
            let ctrl = keycode_of(&map, "Control_L").expect("Control_L mapped");
            let shift = keycode_of(&map, "Shift_L").expect("Shift_L mapped");
            let key = keycode_of(&map, &args[1]).ok_or_else(|| {
                anyhow::anyhow!("keysym {} not mapped", args[1])
            })?;
            fake_key(&conn, ctrl, true);
            fake_key(&conn, shift, true);
            fake_key(&conn, key, true);
            fake_key(&conn, key, false);
            fake_key(&conn, shift, false);
            fake_key(&conn, ctrl, false);
        }
        "type" => {
            for ch in args[1].chars() {
                type_char(&conn, &map, ch)?;
                std::thread::sleep(Duration::from_millis(12));
            }
        }
        "scroll" => {
            let dy: i32 = args[1].parse()?;
            let button = if dy < 0 { 4 } else { 5 };
            for _ in 0..dy.abs().min(20) {
                button_click(&conn, button);
                std::thread::sleep(Duration::from_millis(24));
            }
        }
        "wait" => {
            std::thread::sleep(Duration::from_millis(args[1].parse()?));
        }
        "wins" => {
            let tree = conn.query_tree(root)?.reply()?;
            for win in tree.children.iter() {
                let name = conn.get_property(false, *win, xproto::AtomEnum::WM_NAME, xproto::AtomEnum::STRING, 0, 1024)?.reply()?;
                let title = String::from_utf8_lossy(&name.value).into_owned();
                let geom = conn.get_geometry(*win)?.reply()?;
                let attrs = conn.get_window_attributes(*win)?.reply()?;
                println!(
                    "win {win:#x} '{}': {}x{}+{}+{} map_state={:?}",
                    title,
                    geom.width,
                    geom.height,
                    geom.x,
                    geom.y,
                    attrs.map_state
                );
            }
        }
        "dumpkeys" => {
            for name in ["Return", "Escape", "Tab", "a", "h", "Control_L", "Shift_L", "F11", "Left", "space"] {
                let sym = keysym_by_name(name).unwrap_or(0);
                let code = map.by_sym.get(&sym).copied();
                println!("{name}: keysym={sym:#06x} keycode={code:?}");
            }
            println!("map size: {}", map.by_sym.len());
        }
        "exts" => {
            let reply = conn.list_extensions()?.reply()?;
            for ext in reply.names {
                let name = String::from_utf8_lossy(&ext.name).into_owned();
                println!("{name}");
            }
        }
        other => anyhow::bail!("unknown command: {other}"),
    }
    conn.flush()?;
    Ok(())
}

fn bring_window_front(conn: &RustConnection, root: u32) -> anyhow::Result<()> {
    // Raise and focus any mapped top-level window (the browser is the only
    // client). Without a window manager nothing else ever sets input focus,
    // so keyboard events would be dropped — do it ourselves.
    let tree = conn.query_tree(root)?.reply()?;
    // Pick the largest viewable top-level window (winit keeps a 1x1 helper
    // window around; focusing that one would steal focus from the app).
    let mut best: Option<(u32, u64)> = None;
    for win in tree.children.iter() {
        let geom = match conn.get_geometry(*win) {
            Ok(cookie) => match cookie.reply() {
                Ok(g) => g,
                Err(_) => continue,
            },
            Err(_) => continue,
        };
        let area = geom.width as u64 * geom.height as u64;
        if best.map(|(_, a)| area > a).unwrap_or(true) {
            best = Some((*win, area));
        }
    }
    if let Some((win, _)) = best {
        conn.configure_window(win, &ConfigureWindowAux::new().stack_mode(xproto::StackMode::ABOVE))?;
        conn.set_input_focus(xproto::InputFocus::PARENT, win, 0u32)?;
    }
    conn.flush()?;
    std::thread::sleep(Duration::from_millis(80));
    Ok(())
}

fn warp(conn: &RustConnection, root: u32, x: i16, y: i16) {
    let _ = conn.warp_pointer(0u32, root, 0, 0, 0, 0, x, y);
    let _ = conn.flush();
}

fn button_click(conn: &RustConnection, button: u8) {
    // type 4 = ButtonPress, 5 = ButtonRelease; x/y 0 keeps the warped position.
    let _ = conn.xtest_fake_input(4, button, 0, 0, 0, 0, 0);
    let _ = conn.flush();
    let _ = conn.xtest_fake_input(5, button, 0, 0, 0, 0, 0);
    let _ = conn.flush();
}

fn fake_key(conn: &RustConnection, keycode: u8, press: bool) {
    // type 2 = KeyPress, 3 = KeyRelease.
    let event_type = if press { 2u8 } else { 3u8 };
    let _ = conn.xtest_fake_input(event_type, keycode, 0, 0, 0, 0, 0);
    let _ = conn.flush();
}

fn key_tap(conn: &RustConnection, map: &Keymap, keysym: &str) -> anyhow::Result<()> {
    let code = keycode_of(map, keysym)
        .ok_or_else(|| anyhow::anyhow!("keysym {keysym} not mapped"))?;
    fake_key(conn, code, true);
    fake_key(conn, code, false);
    Ok(())
}

fn modifier_tap(
    conn: &RustConnection,
    map: &Keymap,
    modifier: &str,
    keysym: &str,
) -> anyhow::Result<()> {
    let mod_code = keycode_of(map, modifier).ok_or_else(|| {
        anyhow::anyhow!("keysym {modifier} not mapped")
    })?;
    let key = keycode_of(map, keysym)
        .ok_or_else(|| anyhow::anyhow!("keysym {keysym} not mapped"))?;
    fake_key(conn, mod_code, true);
    fake_key(conn, key, true);
    fake_key(conn, key, false);
    fake_key(conn, mod_code, false);
    Ok(())
}

fn type_char(conn: &RustConnection, map: &Keymap, ch: char) -> anyhow::Result<()> {
    let lower = ch.to_lowercase().next().unwrap_or(ch);
    let lower_sym = char_to_keysym(lower);
    let upper_sym = char_to_keysym(ch);
    if let Some(code) = keycode_of(map, &keysym_name(lower_sym)) {
        if lower == ch {
            fake_key(conn, code, true);
            fake_key(conn, code, false);
            return Ok(());
        }
        // Uppercase / shifted: use Shift.
        let shift = keycode_of(map, "Shift_L").expect("Shift_L mapped");
        fake_key(conn, shift, true);
        fake_key(conn, code, true);
        fake_key(conn, code, false);
        fake_key(conn, shift, false);
        return Ok(());
    }
    if let Some(code) = keycode_of(map, &keysym_name(upper_sym)) {
        let shift = keycode_of(map, "Shift_L").expect("Shift_L mapped");
        fake_key(conn, shift, true);
        fake_key(conn, code, true);
        fake_key(conn, code, false);
        fake_key(conn, shift, false);
        return Ok(());
    }
    // Fall back to raw keysym 0x00ff "dead" area symbols (colons etc. live in
    // the Latin-1 block and are usually reachable via shift).
    anyhow::bail!("no keycode for character {ch:?}")
}

struct Keymap {
    by_sym: HashMap<Keysym, u8>,
}

fn keycode_of(map: &Keymap, name: &str) -> Option<u8> {
    let sym: Keysym = if let Some(rest) = name.strip_prefix("0x") {
        u32::from_str_radix(rest, 16).ok()?
    } else {
        keysym_by_name(name)?
    };
    map.by_sym.get(&sym).copied()
}

fn char_to_keysym(ch: char) -> Keysym {
    // Latin-1: keysym == code point.
    (ch as u32) & 0xff_ff
}

fn keysym_name(sym: u32) -> String {
    format!("0x{sym:04x}")
}

/// Standard keysym names used by this driver.
fn keysym_by_name(name: &str) -> Option<Keysym> {
    let names: &[(&str, u32)] = &[
        ("Return", 0xff0d),
        ("Escape", 0xff1b),
        ("Tab", 0xff09),
        ("BackSpace", 0xff08),
        ("Delete", 0xffff),
        ("Home", 0xff50),
        ("End", 0xff57),
        ("Left", 0xff51),
        ("Right", 0xff53),
        ("Up", 0xff52),
        ("Down", 0xff54),
        ("Prior", 0xff55),
        ("Next", 0xff56),
        ("space", 0x0020),
        ("period", 0x002e),
        ("slash", 0x002f),
        ("colon", 0x003a),
        ("minus", 0x002d),
        ("equal", 0x003d),
        ("underscore", 0x005f),
        ("plus", 0x002b),
        ("at", 0x0040),
        ("numbersign", 0x0023),
        ("percent", 0x0025),
        ("ampersand", 0x0026),
        ("asterisk", 0x002a),
        ("question", 0x003f),
        ("quotedbl", 0x0022),
        ("apostrophe", 0x0027),
        ("parenleft", 0x0028),
        ("parenright", 0x0029),
        ("comma", 0x002c),
        ("semicolon", 0x003b),
        ("less", 0x003c),
        ("greater", 0x003e),
        ("bracketleft", 0x005b),
        ("bracketright", 0x005d),
        ("Control_L", 0xffe3),
        ("Shift_L", 0xffe1),
        ("Alt_L", 0xffe9),
        ("F1", 0xffbe),
        ("F2", 0xffbf),
        ("F3", 0xffc0),
        ("F4", 0xffc1),
        ("F5", 0xffc2),
        ("F6", 0xffc3),
        ("F7", 0xffc4),
        ("F8", 0xffc5),
        ("F9", 0xffc6),
        ("F10", 0xffc7),
        ("F11", 0xffc8),
        ("F12", 0xffc9),
    ];
    names
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, s)| *s)
        .or_else(|| {
            // Single characters: keysym is the code point.
            let mut chars = name.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) if (c as u32) < 0x80 => Some(c as u32),
                _ => None,
            }
        })
}

/// Builds keysym → keycode from the server's keyboard mapping.
fn keymap(conn: &RustConnection) -> anyhow::Result<Keymap> {
    let setup = conn.setup();
    let min = setup.min_keycode as u8;
    let max = setup.max_keycode as u8;
    let count = (max - min + 1) as u8;
    let reply = conn.get_keyboard_mapping(min, count)?.reply()?;
    let syms_per_code = reply.keysyms_per_keycode as usize;
    let mut by_sym: HashMap<Keysym, u8> = HashMap::new();
    for (i, chunk) in reply.keysyms.chunks(syms_per_code.max(1)).enumerate() {
        let keycode = min + i as u8;
        for (level, sym) in chunk.iter().enumerate() {
            if *sym == 0 {
                continue;
            }
            // Prefer the first level (unshifted).
            by_sym.entry(*sym).or_insert(keycode);
            let _ = level;
        }
    }
    // Ensure Latin-1 letters map (some keymaps list only uppercase).
    Ok(Keymap { by_sym })
}
