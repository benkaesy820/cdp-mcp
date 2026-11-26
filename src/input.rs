// The input stack: guards, key names, pointer paths and the action dispatcher.

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};

use crate::cdp;
use crate::scan;
use crate::scan::{frame_click, frame_target, pick};
use crate::scroll;
use crate::wait::{wait_for_text, wait_idle};
use crate::{check_cancel, nap, scope_for, MAX_WAIT_MS};

/// Irreversible actions already performed in this process.
/// Keyed by tab + label + index: a retry loop is caught, a *different* post is
/// not, and a same-labelled button on another tab is a different click.
fn guard() -> &'static Mutex<HashSet<String>> {
    static DONE: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    DONE.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Final confirmations only. A bare "Share" opens a dialog and is not included;
/// "Share now" is, because that is the click that publishes.
pub(crate) fn irreversible(label: &str) -> bool {
    let l = label.trim().to_lowercase();
    ["post", "publish", "share now", "send", "delete", "remove", "pay", "buy",
     "place order", "confirm", "submit order"]
        .iter()
        .any(|w| l == *w || l.starts_with(&format!("{w} ")) || l.ends_with(&format!(" {w}")))
}

/// Does `label` contain `phrase` as whole words? "ok" is in "OK" and "Ok, got
/// it" but not in "Book now" or "Facebook", which a substring match clicked.
pub(crate) fn has_words(label: &str, phrase: &str) -> bool {
    let word = |c: char| c.is_alphanumeric();
    label.match_indices(phrase).any(|(at, m)| {
        let before = label[..at].chars().next_back();
        let after = label[at + m.len()..].chars().next();
        !before.is_some_and(word) && !after.is_some_and(word)
    })
}

/// Refuse a second click on the same final confirmation in one run, unless the
/// caller forced it. Checking and recording are separate: a refused or failed
/// dispatch must not consume the guard for a later, real click.
pub(crate) fn check_irreversible(tab: &str, label: &str, i: u64, force: bool) -> Result<(), String> {
    let key = format!("{tab}|{label}|{i}");
    if guard().lock().unwrap().contains(&key) && !force {
        return Err(format!(
            "'{label}' was already clicked in this run. \
             Verify the result with elements {{ mode: \"delta\" }} first; \
             pass input {{ force: true }} only if you mean to do it twice."
        ));
    }
    Ok(())
}

/// Record a final confirmation only once its dispatch landed.
pub(crate) fn record_irreversible(tab: &str, label: &str, i: u64) {
    guard().lock().unwrap().insert(format!("{tab}|{label}|{i}"));
}

/// The refusal a blocked indexed click reports. One shape for the page and the
/// frame path, so a reason and its target cannot drift apart.
pub(crate) fn blocked_click(
    reason: &str,
    i: u64,
    tag: &Value,
    label: &Value,
    obscured_by: Value,
    in_frame: bool,
) -> String {
    let mut target = json!({ "i": i, "tag": tag, "label": label });
    if in_frame {
        target["inFrame"] = json!(true);
    }
    json!({ "blocked": true, "reason": reason, "target": target, "obscuredBy": obscured_by })
        .to_string()
}

/// What CDP needs to know about one key: DOM `key`, physical `code`, Windows
/// virtual key code, and the text it types (None for keys that type nothing).
pub(crate) fn key_spec(name: &str) -> (String, String, i64, Option<String>) {
    let named = |key: &str, code: &str, vk: i64, text: Option<&str>| {
        (key.to_string(), code.to_string(), vk, text.map(str::to_string))
    };
    match name.to_lowercase().as_str() {
        "enter" | "return" => named("Enter", "Enter", 13, Some("\r")),
        "tab" => named("Tab", "Tab", 9, None),
        "escape" | "esc" => named("Escape", "Escape", 27, None),
        "backspace" => named("Backspace", "Backspace", 8, None),
        "delete" | "del" => named("Delete", "Delete", 46, None),
        "insert" => named("Insert", "Insert", 45, None),
        "space" | " " => named(" ", "Space", 32, Some(" ")),
        "arrowup" | "up" => named("ArrowUp", "ArrowUp", 38, None),
        "arrowdown" | "down" => named("ArrowDown", "ArrowDown", 40, None),
        "arrowleft" | "left" => named("ArrowLeft", "ArrowLeft", 37, None),
        "arrowright" | "right" => named("ArrowRight", "ArrowRight", 39, None),
        "home" => named("Home", "Home", 36, None),
        "end" => named("End", "End", 35, None),
        "pageup" => named("PageUp", "PageUp", 33, None),
        "pagedown" => named("PageDown", "PageDown", 34, None),
        f if f.len() >= 2 && f.starts_with('f') && f[1..].parse::<u8>().is_ok_and(|n| (1..=12).contains(&n)) => {
            let n: i64 = f[1..].parse().unwrap_or(1);
            let k = format!("F{n}");
            (k.clone(), k, 111 + n, None)
        }
        _ => {
            let mut chars = name.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) if c.is_ascii_alphabetic() => {
                    let up = c.to_ascii_uppercase();
                    (c.to_string(), format!("Key{up}"), up as i64, Some(c.to_string()))
                }
                (Some(c), None) if c.is_ascii_digit() => {
                    (c.to_string(), format!("Digit{c}"), c as i64, Some(c.to_string()))
                }
                (Some(c), None) => (c.to_string(), String::new(), 0, Some(c.to_string())),
                _ => (name.to_string(), name.to_string(), 0, None),
            }
        }
    }
}

/// Press and release one key, with modifiers given as "Control+Shift+z".
/// Returns the modifier mask that was applied.
pub(crate) fn press(conn: &mut cdp::Conn, combo: &str) -> Result<i64, String> {
    // "+" alone, or a trailing "++", means the plus key itself.
    let (mods, base) = if combo == "+" {
        ("", "+")
    } else if let Some(m) = combo.strip_suffix("++") {
        (m, "+")
    } else {
        combo.rsplit_once('+').unwrap_or(("", combo))
    };
    let mut mask = 0;
    for m in mods.split('+').filter(|m| !m.is_empty()) {
        mask |= match m.to_lowercase().as_str() {
            "alt" | "option" => 1,
            "control" | "ctrl" => 2,
            "meta" | "cmd" | "command" | "super" => 4,
            "shift" => 8,
            other => return Err(format!("unknown modifier {other:?} in {combo:?}")),
        };
    }
    let (key, code, vk, mut text) = key_spec(base);
    // A shortcut types nothing; Shift alone types the shifted character.
    if mask & (1 | 2 | 4) != 0 {
        text = None;
    } else if mask & 8 != 0 {
        text = text.map(|t| t.to_uppercase());
    }
    let key = match (&text, mask & 8 != 0) {
        (Some(t), true) if key.chars().count() == 1 => t.clone(),
        _ => key,
    };
    let mut down = json!({
        "type": if text.is_some() { "keyDown" } else { "rawKeyDown" },
        "modifiers": mask, "key": key, "code": code,
        "windowsVirtualKeyCode": vk, "nativeVirtualKeyCode": vk,
    });
    if let Some(t) = &text {
        down["text"] = json!(t);
        down["unmodifiedText"] = json!(t);
    }
    let up = json!({
        "type": "keyUp", "modifiers": mask, "key": key, "code": code,
        "windowsVirtualKeyCode": vk, "nativeVirtualKeyCode": vk,
    });
    conn.call_many(&[("Input.dispatchKeyEvent", down), ("Input.dispatchKeyEvent", up)])?;
    Ok(mask)
}

pub(crate) fn click_at(conn: &mut cdp::Conn, x: f64, y: f64) -> Result<(), String> {
    // The three events are pipelined: the browser still sees them in order, but
    // there is one round trip instead of three.
    conn.call_many(&[
        ("Input.dispatchMouseEvent", json!({ "type": "mouseMoved", "x": x, "y": y })),
        ("Input.dispatchMouseEvent", json!({ "type": "mousePressed", "x": x, "y": y,
                                            "button": "left", "clickCount": 1, "buttons": 1 })),
        ("Input.dispatchMouseEvent", json!({ "type": "mouseReleased", "x": x, "y": y,
                                            "button": "left", "clickCount": 1, "buttons": 0 })),
    ])?;
    Ok(())
}

/// A click with a non-left button, a click count or modifiers.
pub(crate) fn click_precise(conn: &mut cdp::Conn, x: f64, y: f64, button: &str, count: u64, mods: i64) -> Result<(), String> {
    let held = match button { "right" => 2, "middle" => 4, _ => 1 };
    conn.call_many(&[
        ("Input.dispatchMouseEvent", json!({ "type": "mouseMoved", "x": x, "y": y, "modifiers": mods })),
        ("Input.dispatchMouseEvent", json!({ "type": "mousePressed", "x": x, "y": y,
            "button": button, "clickCount": count, "buttons": held, "modifiers": mods })),
        ("Input.dispatchMouseEvent", json!({ "type": "mouseReleased", "x": x, "y": y,
            "button": button, "clickCount": count, "buttons": 0, "modifiers": mods })),
    ])?;
    Ok(())
}

/// Click element `i`: resolved, scrolled into view and measured in one
/// evaluation, then clicked. Returns what `pick` saw.
pub(crate) fn click_index(conn: &mut cdp::Conn, scope: Option<&str>, i: u64) -> Result<Value, String> {
    let at = pick(conn, scope, i, true, None)?;
    let x = at.get("x").and_then(Value::as_f64).unwrap_or(0.0);
    let y = at.get("y").and_then(Value::as_f64).unwrap_or(0.0);
    click_at(conn, x, y)?;
    Ok(at)
}

/// The log entry for a click by index: where it went, and a warning when
/// something else was on top of the target.
pub(crate) fn click_log(at: &Value) -> Value {
    let mut c = json!({ "i": at.get("i"), "x": at.get("x"), "y": at.get("y") });
    if at.get("scrolled").and_then(Value::as_bool) == Some(true) {
        c["scrolled"] = json!(true);
    }
    if at.get("hit").and_then(Value::as_bool) == Some(false) {
        c["covered"] = at.get("blocker").cloned().unwrap_or(json!(true));
    }
    // The index moved because the page changed above it; the click went to
    // the element the caller saw, at its new index.
    if let Some(r) = at.get("remapped") {
        c["remapped"] = r.clone();
    }
    c
}


/// Resolve `{i}` or `{x,y}` to a screen point. An index is scrolled into
/// view first, so the point is on screen where input can reach it.
pub(crate) fn point_of(conn: &mut cdp::Conn, scope: Option<&str>, v: &Value) -> Result<(f64, f64), String> {
    if let Some(i) = v.get("i").and_then(Value::as_u64) {
        let at = pick(conn, scope, i, true, None)?;
        return Ok((
            at.get("x").and_then(Value::as_f64).unwrap_or(0.0),
            at.get("y").and_then(Value::as_f64).unwrap_or(0.0),
        ));
    }
    let x = v.get("x").and_then(Value::as_f64).ok_or("point needs i or x,y")?;
    let y = v.get("y").and_then(Value::as_f64).ok_or("point needs i or x,y")?;
    Ok((x, y))
}

/// Modifier bitmask for CDP: Alt=1, Ctrl=2, Meta=4, Shift=8.
pub(crate) fn mod_mask(v: Option<&Value>) -> i64 {
    let mut m = 0;
    if let Some(arr) = v.and_then(Value::as_array) {
        for x in arr.iter().filter_map(Value::as_str) {
            match x.to_lowercase().as_str() {
                "alt" => m |= 1,
                "control" | "ctrl" => m |= 2,
                "meta" | "cmd" | "super" => m |= 4,
                "shift" => m |= 8,
                _ => {}
            }
        }
    }
    m
}

/// A pointer move in small steps: pages that track drag gestures (canvas, lists,
/// sliders) ignore a single jump from A to B.
pub(crate) fn drag_path(conn: &mut cdp::Conn, from: (f64, f64), to: (f64, f64), steps: u32) -> Result<(), String> {
    let steps = steps.clamp(2, 40);
    // One round trip per move, deliberately. Pipelining the whole path into a
    // single turn is 5x faster but Chromium coalesces mouse-moved events that
    // arrive together: measured on a 9-point path, the page saw 2 points and a
    // release. A slider, a canvas or a signature field needs the path, so each
    // step keeps its own turn. `steps` is the knob when a slow page makes the
    // cost visible (a tab with a <video> element measured ~100-500 ms a step).
    conn.call_many(&[("Input.dispatchMouseEvent",
        json!({ "type": "mouseMoved", "x": from.0, "y": from.1 }))])?;
    conn.call_many(&[("Input.dispatchMouseEvent",
        json!({ "type": "mousePressed", "x": from.0, "y": from.1,
                "button": "left", "clickCount": 1, "buttons": 1 }))])?;
    for s in 1..=steps {
        let f = s as f64 / steps as f64;
        conn.call_many(&[("Input.dispatchMouseEvent",
            json!({ "type": "mouseMoved",
                    "x": from.0 + (to.0 - from.0) * f,
                    "y": from.1 + (to.1 - from.1) * f,
                    "button": "left", "buttons": 1 }))])?;
    }
    conn.call_many(&[("Input.dispatchMouseEvent",
        json!({ "type": "mouseReleased", "x": to.0, "y": to.1,
                "button": "left", "clickCount": 1, "buttons": 0 }))])?;
    Ok(())
}

/// A multiple selection matches when the sets are equal. The page reports its
/// options in its own order, and a repeated request value is the same request.
pub(crate) fn selection_matches(want: &[String], got: &Value) -> bool {
    let mut want: Vec<&str> = want.iter().map(String::as_str).collect();
    let mut have: Vec<&str> = got
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    want.sort_unstable();
    want.dedup();
    have.sort_unstable();
    have.dedup();
    want == have
}

/// `select.value` as the action takes it: one string, or a non-empty array of
/// strings for a multiple select.
pub(crate) fn select_values(v: Option<&Value>) -> Result<Value, String> {
    match v {
        Some(Value::String(s)) => Ok(Value::String(s.clone())),
        Some(Value::Array(a)) if !a.is_empty() => {
            let mut out = Vec::with_capacity(a.len());
            for x in a {
                out.push(Value::String(
                    x.as_str().ok_or("select values must be strings")?.to_string(),
                ));
            }
            Ok(Value::Array(out))
        }
        Some(Value::Array(_)) => Err("select needs at least one value".into()),
        Some(_) => Err("select value must be a string or an array of strings".into()),
        None => Err("select needs value".into()),
    }
}

/// A failure that is about the request, not about a document going away.
fn clean_failure(e: &str) -> bool {
    e.starts_with("no element with index")
        || e.starts_with("wait:")
        || e.starts_with("select failed:")
        || e.starts_with("upload:")
        || e.contains("\"blocked\":true")
        || e.contains("was already clicked in this run")
}

/// Compare what the page selected against what was asked for, and shape the log
/// entry. Shared by the page path and the frame path so their answers cannot
/// drift.
fn selected_entry(i: u64, value: &Value, got: Value, in_frame: bool) -> Result<Value, String> {
    match value {
        Value::Array(want) => {
            let want: Vec<String> =
                want.iter().filter_map(Value::as_str).map(str::to_string).collect();
            if !selection_matches(&want, &got) {
                return Err(format!("select failed: asked for {want:?}, the page has {got}"));
            }
        }
        Value::String(want) => {
            if got.as_str() != Some(want.as_str()) {
                return Err(format!(
                    "select failed: the <select> has no option {want:?} (it is {got})"
                ));
            }
        }
        _ => {}
    }
    let mut entry = json!({ "i": i, "value": got });
    if in_frame {
        entry["inFrame"] = json!(true);
    }
    Ok(entry)
}

pub(crate) fn run_input(
    conn: &mut cdp::Conn,
    tab: &str,
    actions: &[Value],
    force: bool,
    scope: Option<&str>,
    log: &mut Vec<Value>,
) -> Result<(), String> {
    for a in actions {
        check_cancel()?;
        // The failing action's own navigation is what a swallow may excuse; reset
        // so an earlier action cannot cover for a later failure.
        conn.contexts_cleared = false;
        let obj = a.as_object().ok_or("each action must be an object")?;
        // A dialog blocks the renderer. Answering it is the only action that can
        // proceed; anything else would wait out its socket timeout and change
        // nothing, so the batch stops here with what already ran.
        if conn.dialog.is_some() && obj.get("dialog").is_none() {
            return Ok(());
        }
        if let Some(d) = obj.get("dialog") {
            // The one action allowed while the renderer is blocked. One answer
            // per call: a chained alert is reported, not auto-dismissed.
            let accept = d.get("accept").and_then(Value::as_bool).unwrap_or(true);
            let text = d.get("text").and_then(Value::as_str);
            let next = conn.handle_dialog(accept, text)?;
            log.push(json!({ "dialogAnswered": { "accept": accept } }));
            // A chained dialog can open as the first one closes: its event may
            // land just after the reply, so read what has arrived before
            // reporting. Answering it is the caller's decision, not ours.
            let _ = conn.drain_arrived();
            if next.is_some() || conn.dialog.is_some() {
                return Ok(());
            }
        } else if let Some(c) = obj.get("click") {
            let button = c.get("button").and_then(Value::as_str).unwrap_or("left").to_string();
            let count = c.get("count").and_then(Value::as_u64).unwrap_or(1);
            let mods = mod_mask(c.get("modifiers"));
            if let Some(i) = c.get("i").and_then(Value::as_u64) {
                // A per-action scope replaces the ambient one; an empty selector
                // is no scope at all, not a scope that matches everything.
                let scope = match c.get("within").and_then(Value::as_str) {
                    Some(w) if w.trim().is_empty() => None,
                    Some(w) => Some(w.trim()),
                    None => scope,
                };
                // An element inside a cross-origin frame lives in another
                // renderer: a click at its page coordinates lands in the frame's
                // box but the frame's own document receives it at LOCAL
                // coordinates, so the event has to be sent to its session.
                if let Some(target) = frame_target(conn, tab, scope, i, force)? {
                    let (lx, ly) = (target.0, target.1);
                    let label = target.3.clone();
                    let clicked = if button != "left" || count > 1 || mods != 0 {
                        frame_click(conn, &target.2, lx, ly, &button, count, mods as u32)
                    } else {
                        frame_click(conn, &target.2, lx, ly, "left", 1, 0)
                    };
                    // A handler that opened a dialog blocks the renderer, so the
                    // click's outcome is not known yet: report the dialog instead
                    // of an error, and mark the entry as unsettled.
                    if let Err(e) = clicked {
                        if conn.dialog.is_some() {
                            if irreversible(&label) {
                                record_irreversible(tab, &label, i);
                            }
                            log.push(json!({ "clicked": { "i": i, "inFrame": true, "x": lx, "y": ly, "dialog": true } }));
                            return Ok(());
                        }
                        return Err(e);
                    }
                    if irreversible(&label) {
                        record_irreversible(tab, &label, i);
                    }
                    log.push(json!({ "clicked": { "i": i, "inFrame": true, "x": lx, "y": ly } }));
                    continue;
                }
                // Resolve first (one evaluation), then check the label before
                // clicking: a repeat of a final confirmation is the failure that
                // bites an unattended run.
                let mut at = pick(conn, scope, i, true, None)?;
                let mut cleared = Value::Null;
                if !force {
                    let mut reason = if at.get("connected").and_then(Value::as_bool) == Some(false) {
                        Some("detached")
                    } else if at.get("zero").and_then(Value::as_bool) == Some(true) {
                        Some("zero_size")
                    } else if at.get("disabled").and_then(Value::as_bool) == Some(true) {
                        Some("disabled")
                    } else if at.get("hit").and_then(Value::as_bool) == Some(false) {
                        Some("obscured")
                    } else {
                        None
                    };
                    // A real click would land on the overlay. If that overlay is
                    // an advertisement, hide it and re-test, then click the
                    // revealed target. Anything else keeps its refusal.
                    if reason == Some("obscured") {
                        if let Ok(value) = scan::pick_clear_ads(conn, scope, i) {
                            if value.get("hit").and_then(Value::as_bool) == Some(true) {
                                cleared = value.get("cleared").cloned().unwrap_or(Value::Null);
                                at = value;
                                reason = None;
                            }
                        }
                    }
                    if let Some(reason) = reason {
                        return Err(blocked_click(
                            reason,
                            i,
                            at.get("tag").unwrap_or(&Value::Null),
                            at.get("label").unwrap_or(&Value::Null),
                            at.get("blocker").cloned().unwrap_or(Value::Null),
                            false,
                        ));
                    }
                }
                let guard_label = at.get("label").and_then(Value::as_str)
                    .filter(|label| irreversible(label)).map(str::to_string);
                if let Some(label) = &guard_label {
                    check_irreversible(tab, label, i, force)?;
                }
                let x = at.get("x").and_then(Value::as_f64).unwrap_or(0.0);
                let y = at.get("y").and_then(Value::as_f64).unwrap_or(0.0);
                let clicked = if button != "left" || count > 1 || mods != 0 {
                    click_precise(conn, x, y, &button, count, mods)
                } else {
                    click_at(conn, x, y)
                };
                if let Err(e) = clicked {
                    if conn.dialog.is_some() {
                        if let Some(label) = &guard_label {
                            record_irreversible(tab, label, i);
                        }
                        log.push(json!({ "clicked": { "i": i, "dialog": true } }));
                        return Ok(());
                    }
                    return Err(e);
                }
                if let Some(label) = &guard_label {
                    record_irreversible(tab, label, i);
                }
                let mut hit = json!({ "clicked": click_log(&at) });
                if !cleared.is_null() {
                    // The page changed to make this click possible; say so
                    // rather than reporting a clean click on a page that moved.
                    hit["cleared"] = cleared;
                }
                log.push(hit);
            } else {
                let (x, y) = point_of(conn, scope, c)?;
                let clicked = if button != "left" || count > 1 || mods != 0 {
                    click_precise(conn, x, y, &button, count, mods)
                } else {
                    click_at(conn, x, y)
                };
                if let Err(e) = clicked {
                    if conn.dialog.is_some() {
                        log.push(json!({ "clicked": { "x": x, "y": y, "dialog": true } }));
                        return Ok(());
                    }
                    return Err(e);
                }
                log.push(json!({ "clicked": { "x": x, "y": y, "button": button, "count": count, "modifiers": mods } }));
            }
        } else if let Some(t) = obj.get("insert").and_then(Value::as_str) {
            conn.call("Input.insertText", json!({ "text": t }))?;
            log.push(json!({ "inserted": t.chars().count() }));
        } else if let Some(k) = obj.get("key").and_then(Value::as_str) {
            // Enter can submit a form, and a submit handler can open a dialog.
            let mask = match press(conn, k) {
                Ok(m) => m,
                Err(e) => {
                    if conn.dialog.is_some() {
                        log.push(json!({ "key": k, "dialog": true }));
                        return Ok(());
                    }
                    return Err(e);
                }
            };
            if mask != 0 {
                log.push(json!({ "key": k, "modifiers": mask }));
            } else {
                log.push(json!({ "key": k }));
            }
        } else if let Some(s) = obj.get("scroll") {
            let (dx, dy) = scroll::deltas(
                s.get("dx").and_then(Value::as_f64),
                s.get("dy").and_then(Value::as_f64),
            )?;
            // A real wheel goes where the pointer is: an element, a point, or the
            // middle of the viewport for a plain page scroll.
            let (x, y) = if let Some(i) = s.get("i").and_then(Value::as_u64) {
                let at = pick(conn, scope, i, true, None)?;
                (
                    at.get("x").and_then(Value::as_f64).unwrap_or(0.0),
                    at.get("y").and_then(Value::as_f64).unwrap_or(0.0),
                )
            } else if let (Some(x), Some(y)) = (
                s.get("x").and_then(Value::as_f64),
                s.get("y").and_then(Value::as_f64),
            ) {
                (x, y)
            } else {
                let v = conn.eval("[innerWidth / 2, innerHeight / 2]")?;
                (
                    v.get(0).and_then(Value::as_f64).unwrap_or(0.0),
                    v.get(1).and_then(Value::as_f64).unwrap_or(0.0),
                )
            };
            log.push(json!({ "scrolled": scroll::send(conn, x, y, dx, dy)? }));
        } else if let Some(d) = obj.get("dismiss") {
            let prefer = d.get("prefer").and_then(Value::as_str).unwrap_or("reject").to_lowercase();
            // Closer indexes come from a whole-document scan, so they are
            // clicked in that same (unscoped) scan below.
            let scan_v = conn.eval(&scan::dismiss_scan_js())?;
            let overlays = scan_v.get("overlays").and_then(Value::as_array).cloned().unwrap_or_default();
            let mut cands: Vec<(String, i64)> = Vec::new();
            for o in &overlays {
                if let Some(cs) = o.get("closers").and_then(Value::as_array) {
                    for c in cs {
                        if let (Some(l), Some(idx)) = (
                            c.get("label").and_then(Value::as_str),
                            c.get("i").and_then(Value::as_i64),
                        ) {
                            cands.push((l.to_lowercase(), idx));
                        }
                    }
                }
            }
            if cands.is_empty() {
                log.push(json!({ "dismissed": null, "overlays": overlays.len(),
                                 "note": "no dismiss buttons found" }));
                continue;
            }
            // Policy: refuse cookies unless refusing is not offered. "Only
            // necessary" and "decline" are refusals; "accept all" is the last resort.
            let refuse = ["reject", "decline", "block", "deny", "refuse", "necessary", "essential",
                          "only needed", "do not sell", "manage"];
            let acceptish = ["accept", "agree", "allow", "got it", "ok", "continue", "close",
                             "dismiss", "later", "not now", "×", "✕"];
            let want: &[&str] = if prefer == "accept" { &acceptish } else { &refuse };
            let pick_c = cands
                .iter()
                .find(|(l, _)| want.iter().any(|w| has_words(l, w)))
                .or_else(|| cands.iter().find(|(l, _)| acceptish.iter().any(|w| has_words(l, w))));
            match pick_c {
                Some((label, idx)) => {
                    let i = *idx as u64;
                    click_index(conn, None, i)?;
                    log.push(json!({ "dismissed": { "label": label, "i": i, "preference": prefer } }));
                }
                None => log.push(json!({ "dismissed": null,
                                         "candidates": cands.iter().map(|(l, _)| l.clone()).collect::<Vec<_>>() })),
            }
        } else if let Some(h) = obj.get("hover") {
            let frame = match h.get("i").and_then(Value::as_u64) {
                // A scoped list holds no frame elements, exactly as for clicks.
                Some(i) if scope.is_none() => scan::frame_pick(conn, tab, i, true, None)?,
                _ => None,
            };
            if let Some(f) = frame {
                let x = f.get("lx").and_then(Value::as_f64).unwrap_or(0.0);
                let y = f.get("ly").and_then(Value::as_f64).unwrap_or(0.0);
                let session = f.get("session").and_then(Value::as_str).unwrap_or_default();
                conn.call_in(Some(session), "Input.dispatchMouseEvent",
                    json!({ "type": "mouseMoved", "x": x, "y": y }))?;
                log.push(json!({ "hovered": { "x": x, "y": y, "inFrame": true } }));
            } else {
                let (x, y) = point_of(conn, scope, h)?;
                conn.call_many(&[("Input.dispatchMouseEvent", json!({ "type": "mouseMoved", "x": x, "y": y }))])?;
                log.push(json!({ "hovered": { "x": x, "y": y } }));
            }
        } else if let Some(d) = obj.get("drag") {
            let from = point_of(conn, scope, d.get("from").ok_or("drag needs from")?)?;
            let to = point_of(conn, scope, d.get("to").ok_or("drag needs to")?)?;
            let steps = d.get("steps").and_then(Value::as_u64).unwrap_or(12) as u32;
            drag_path(conn, from, to, steps)?;
            log.push(json!({ "dragged": { "from": from, "to": to, "steps": steps } }));
        } else if let Some(tp) = obj.get("tap") {
            // Touch input, for pages that only respond to touch (mobile layouts,
            // canvas apps). One finger, press and release at the same point.
            let (x, y) = point_of(conn, scope, tp)?;
            conn.call_many(&[
                ("Input.dispatchTouchEvent", json!({ "type": "touchStart",
                    "touchPoints": [{ "x": x, "y": y, "radiusX": 12, "radiusY": 12, "force": 1 }] })),
                ("Input.dispatchTouchEvent", json!({ "type": "touchEnd", "touchPoints": [] })),
            ])?;
            log.push(json!({ "tapped": { "x": x, "y": y } }));
        } else if let Some(s) = obj.get("swipe") {
            let from = point_of(conn, scope, s.get("from").ok_or("swipe needs from")?)?;
            let to = point_of(conn, scope, s.get("to").ok_or("swipe needs to")?)?;
            let steps = s.get("steps").and_then(Value::as_u64).unwrap_or(10).clamp(1, 40);
            // One round trip per move, like drag_path: moves that arrive
            // together are coalesced (measured: 10 sent, the page saw 2), and a
            // gesture recogniser needs the path, not just the ends.
            conn.call("Input.dispatchTouchEvent", json!({ "type": "touchStart",
                "touchPoints": [{ "x": from.0, "y": from.1, "radiusX": 12, "radiusY": 12, "force": 1 }] }))?;
            for n in 1..=steps {
                let f = n as f64 / steps as f64;
                let x = from.0 + (to.0 - from.0) * f;
                let y = from.1 + (to.1 - from.1) * f;
                conn.call("Input.dispatchTouchEvent", json!({ "type": "touchMove",
                    "touchPoints": [{ "x": x, "y": y, "radiusX": 12, "radiusY": 12, "force": 1 }] }))?;
            }
            conn.call("Input.dispatchTouchEvent", json!({ "type": "touchEnd", "touchPoints": [] }))?;
            log.push(json!({ "swiped": { "from": from, "to": to } }));
        } else if let Some(v) = obj.get("select") {
            // <select> by index: setting .value directly works where a click menu
            // would need a second overlay navigation.
            let i = v.get("i").and_then(Value::as_u64).ok_or("select needs i")?;
            let value = select_values(v.get("value"))?;
            // A frame element is a different document: resolve it there, live, and
            // validate against the answer that document gave.
            let frame = if scope.is_none() {
                scan::frame_pick(conn, tab, i, false, Some(&value))?
            } else {
                None
            };
            let entry = if let Some(f) = frame {
                if let Some(e) = f.get("selectError").and_then(Value::as_str) {
                    return Err(format!("select failed: {e}"));
                }
                let got = f.get("selected").cloned().unwrap_or(Value::Null);
                // What the page resolved the request to: visible text names an
                // option as well as its value does.
                let asked = f.get("asked").cloned().unwrap_or_else(|| value.clone());
                selected_entry(i, &asked, got, true)?
            } else {
                let at = pick(conn, scope, i, false, Some(&value))?;
                if let Some(e) = at.get("selectError").and_then(Value::as_str) {
                    return Err(format!("select failed: {e}"));
                }
                let got = at.get("selected").cloned().unwrap_or(Value::Null);
                let asked = at.get("asked").cloned().unwrap_or_else(|| value.clone());
                selected_entry(i, &asked, got, false)?
            };
            log.push(json!({ "selected": entry }));
        } else if let Some(w) = obj.get("wait") {
            let ms = w.get("ms").and_then(Value::as_u64).unwrap_or(15_000);
            if ms > MAX_WAIT_MS {
                return Err(format!("wait ms must be at most {MAX_WAIT_MS}"));
            }
            if w.get("idle").and_then(Value::as_bool).unwrap_or(false) {
                let started = std::time::Instant::now();
                let w = wait_idle(conn, ms, 300)?;
                let mut e = json!({ "waitedFor": "idle", "ms": started.elapsed().as_millis() as u64 });
                w.put(&mut e);
                log.push(e);
                continue;
            }
            let text = w.get("text").and_then(Value::as_str).ok_or("wait needs text, or idle:true")?;
            let started = std::time::Instant::now();
            if !wait_for_text(conn, text, ms)? {
                return Err(format!("wait: text not found within {ms}ms: {text}"));
            }
            log.push(json!({ "waitedFor": text, "ms": started.elapsed().as_millis() as u64 }));
        } else if let Some(s) = obj.get("sleep") {
            let ms = s.get("ms").and_then(Value::as_u64).unwrap_or(300);
            if ms > MAX_WAIT_MS {
                return Err(format!("sleep ms must be at most {MAX_WAIT_MS}"));
            }
            nap(ms)?;
            log.push(json!({ "slept": ms }));
        } else {
            return Err(format!("unknown action: {a}"));
        }
        if conn.contexts_cleared {
            conn.nav_batch = true;
        }
    }
    Ok(())
}

/// One `input` call: the actions, the optional settle window, and the fields
/// that say what the page did. Split out so the dialog-aware connection guard
/// can be chosen without duplicating the body.
pub(crate) fn run_batch(
    conn: &mut cdp::Conn,
    tab_id: &str,
    actions: &[Value],
    announce: bool,
    force: bool,
    settle_ms: u64,
    managed: Option<&std::path::Path>,
) -> Result<Value, String> {
    match managed {
        Some(dir) => {
            // Armed right before the actions and put back on every exit path: a
            // human's own downloads must never be hijacked into our folder.
            conn.begin_downloads(&dir.display().to_string())?;
            let out = run_batch_inner(
                conn, tab_id, actions, announce, force, settle_ms, Some(dir),
            );
            conn.end_downloads();
            out
        }
        None => run_batch_inner(conn, tab_id, actions, announce, force, settle_ms, None),
    }
}

fn run_batch_inner(
    conn: &mut cdp::Conn,
    tab_id: &str,
    actions: &[Value],
    announce: bool,
    force: bool,
    settle_ms: u64,
    managed: Option<&std::path::Path>,
) -> Result<Value, String> {
    let scope = scope_for(tab_id);
    conn.nav_batch = false;
    let mut navigated = false;
    let mut opened_tab: Option<Value> = None;
    let mut note: Option<String> = None;
    // A batch that answers a dialog must not read the page first, and neither
    // must any batch while one is open: the renderer is blocked. The browser
    // process still answers Target.getTargetInfo.
    // Only a page that is already blocked reaches here with a dialog action
    // first (with_conn_dialog is only used for that shape), so the live state is
    // the whole answer.
    let blocked_before = conn.dialog.is_some();
    // A settle window is opt-in: enabling Network on every click would add
    // latency to the common case, and most actions are not being debugged.
    let mut settle_note: Option<String> = None;
    let mut log: Vec<Value> = Vec::new();
    let (errors, downloads) = if settle_ms > 0 {
        // A blocked renderer cannot answer `Runtime.enable`/`Network.enable`
        // either: measured, arming receipts there dismissed the dialog before
        // the answer was sent. Leave them off; the post-action URL comes from
        // the browser process instead.
        let url = if blocked_before {
            String::new()
        } else {
            conn.eval("location.href")
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default()
        };
        // The caller asked for diagnostics. If they cannot be armed, say so:
        // an empty `errors` and no `downloads` reads as "nothing happened".
        let on = if blocked_before {
            // The dialog has to be answered before Network can be enabled, so
            // the window is skipped. A managed download still lands (the
            // behavior is armed), but it cannot be renamed or reported here.
            settle_note = Some("settle diagnostics skipped: the page was blocked by a dialog when the call started".to_string());
            false
        } else if url.is_empty() {
            settle_note = Some("settle diagnostics skipped: the page URL could not be read".to_string());
            false
        } else {
            match conn.begin_receipts(&url, tab_id) {
                Ok(()) => true,
                Err(e) => {
                    settle_note = Some(format!("settle diagnostics unavailable: {e}"));
                    false
                }
            }
        };
        conn.contexts_cleared = false;
        let result = run_input(conn, tab_id, actions, force, scope.as_deref(), &mut log);
        let nav_failed = conn.contexts_cleared;
        let outcome = if on {
            if result.is_err() {
                // Nothing was dispatched cleanly; do not make the caller wait
                // out the quiet window as well.
                conn.settle_receipts(0, 0)
            } else {
                conn.settle_receipts(250, settle_ms)
            }
        } else {
            cdp::ReceiptOutcome::default()
        };
        navigated = conn.nav_batch || conn.contexts_cleared;
        opened_tab = outcome.opened_tab;
        let downloads = outcome.downloads;
        let errors = outcome.entries;
        if let Err(e) = result {
            // An action this wrapper does not itemise can still be interrupted
            // by a dialog; that is an outcome, not a tool error, and the log
            // keeps what did run before it.
            if conn.dialog.is_some() {
                note = Some(format!("{e} — a dialog is open"));
            } else if nav_failed && !clean_failure(&e) && !conn.dead {
                // A click that navigated can fail on the trailing synthetic
                // event. A refusal, a bad index or a lost socket is not that.
                note = Some(e);
            } else {
                return Err(e);
            }
        }
        (errors, downloads)
    } else {
        let result = run_input(conn, tab_id, actions, force, scope.as_deref(), &mut log);
        if let Err(e) = result {
            if conn.dialog.is_some() {
                note = Some(format!("{e} — a dialog is open"));
            } else {
                return Err(e);
            }
        }
        (Vec::new(), Vec::new())
    };
    // Post-action evals would sit behind a dialog; the browser-process URL works.
    let blocked_after = conn.dialog.is_some();
    // The overlays and the URL in one evaluation: both are wanted together, and a
    // second round trip for `location.href` bought nothing.
    let (overlays, mut url) = if announce && !blocked_after {
        match conn.eval(&format!("[{}, location.href]", scan::OVERLAYS_QUICK_JS)) {
            Ok(v) => (
                v.get(0).cloned().unwrap_or(json!([])),
                v.get(1).cloned().unwrap_or(Value::Null),
            ),
            // The action already happened; a failed look at it is not the result.
            Err(_) => (json!([]), Value::Null),
        }
    } else {
        (json!([]), Value::Null)
    };
    if navigated && !blocked_after && url.is_null() {
        if let Ok(u) = conn.eval("location.href") {
            if let Some(s) = u.as_str() {
                url = json!(s);
            }
        }
    }
    if url.is_null() && blocked_after {
        if let Some(s) = conn.target_url() {
            url = json!(s);
        }
    }
    let mut out = json!({ "tab": tab_id, "url": url, "log": log, "overlays": overlays });
    if navigated {
        out["navigated"] = json!(true);
    }
    if let Some(t) = opened_tab {
        out["openedTab"] = t;
    }
    if let Some(s) = settle_note {
        note = Some(match note {
            Some(n) => format!("{n}; {s}"),
            None => s,
        });
    }
    if let Some(n) = note {
        out["note"] = json!(n);
    }
    if !errors.is_empty() {
        out["errors"] = json!(errors);
    }
    if !downloads.is_empty() {
        // Managed mode moves a completed file out of its guid name and only then
        // calls it completed; plain mode reports what the events said.
        let mut downloads = downloads;
        if let Some(dir) = managed {
            finalize_downloads(dir, &mut downloads);
        }
        out["downloads"] = json!(downloads);
    }
    if let Some(d) = conn.dialog.clone() {
        out["dialog"] = d;
    }
    Ok(out)
}

/// A suggested filename with the directories stripped. The browser supplies this
/// from a remote header, so `../../etc/passwd` must never reach a path join.
fn sanitize_filename(suggested: &str, fallback: &str) -> String {
    // Both separators, on every platform: a Windows-style header is still a
    // traversal string when it is handled on Linux.
    let name = suggested.rsplit(['/', '\\']).next().unwrap_or("").trim();
    if name.is_empty() || name == "." || name == ".." {
        fallback.to_string()
    } else {
        name.to_string()
    }
}

/// Never overwrite: the same download twice gives `report.txt`, `report-1.txt`.
fn unique_path(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let path = std::path::Path::new(name);
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or(name);
    let ext = path.extension().and_then(|e| e.to_str());
    for n in 1..1000 {
        let candidate = match ext {
            Some(e) => format!("{stem}-{n}.{e}"),
            None => format!("{stem}-{n}"),
        };
        let p = dir.join(candidate);
        if !p.exists() {
            return p;
        }
    }
    dir.join(format!("{stem}-{}", std::process::id()))
}

/// Move each completed download out of `allowAndName`'s guid name and report a
/// path only when the file was found and put in place.
fn finalize_downloads(dir: &std::path::Path, downloads: &mut [Value]) {
    for d in downloads.iter_mut() {
        if d.get("status").and_then(Value::as_str) != Some("completed") {
            continue;
        }
        let guid = d.get("guid").and_then(Value::as_str).unwrap_or("").to_string();
        let source = dir.join(&guid);
        if guid.is_empty() || !source.is_file() {
            d["status"] = json!("missing");
            continue;
        }
        let suggested = d.get("filename").and_then(Value::as_str).unwrap_or("");
        let target = unique_path(dir, &sanitize_filename(suggested, &guid));
        match std::fs::rename(&source, &target) {
            Ok(()) => {
                let bytes = std::fs::metadata(&target).map(|m| m.len()).unwrap_or(0);
                d["filename"] = json!(target.file_name().and_then(|n| n.to_str()).unwrap_or(""));
                d["path"] = json!(target.display().to_string());
                d["bytes"] = json!(bytes);
            }
            Err(_) => {
                d["status"] = json!("missing");
                d["note"] = json!("cannot move the download into place");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggested_filenames_cannot_escape_the_folder() {
        assert_eq!(sanitize_filename("../../etc/passwd", "g1"), "passwd");
        assert_eq!(sanitize_filename(r"..\..\windows\system32\evil.exe", "g1"), "evil.exe");
        assert_eq!(sanitize_filename("/abs/report.txt", "g1"), "report.txt");
        assert_eq!(sanitize_filename("", "g1"), "g1");
        assert_eq!(sanitize_filename("..", "g1"), "g1");
        assert_eq!(sanitize_filename("   ", "g1"), "g1");
    }

    #[test]
    fn a_second_download_never_overwrites_the_first() {
        let dir = std::env::temp_dir().join(format!("cdp-dl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let first = unique_path(&dir, "report.txt");
        assert_eq!(first.file_name().unwrap(), "report.txt");
        std::fs::write(&first, b"x").unwrap();
        let second = unique_path(&dir, "report.txt");
        assert_eq!(second.file_name().unwrap(), "report-1.txt");
        std::fs::write(&second, b"y").unwrap();
        assert_eq!(unique_path(&dir, "report.txt").file_name().unwrap(), "report-2.txt");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_completed_download_is_reported_only_when_it_is_on_disk() {
        let dir = std::env::temp_dir().join(format!("cdp-dl-fin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("g1"), b"download-body-123").unwrap();
        let mut ds = vec![json!({
            "guid": "g1", "status": "completed", "filename": "../report.txt" })];
        finalize_downloads(&dir, &mut ds);
        assert_eq!(ds[0]["status"], "completed");
        assert_eq!(ds[0]["filename"], "report.txt");
        assert_eq!(ds[0]["bytes"], 17);
        assert!(std::path::Path::new(ds[0]["path"].as_str().unwrap()).is_file());

        let mut gone = vec![json!({
            "guid": "g2", "status": "completed", "filename": "b.txt" })];
        finalize_downloads(&dir, &mut gone);
        assert_eq!(gone[0]["status"], "missing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn keys_carry_key_code_and_text() {
        assert_eq!(key_spec("Enter"), ("Enter".into(), "Enter".into(), 13, Some("\r".into())));
        assert_eq!(key_spec("a"), ("a".into(), "KeyA".into(), 65, Some("a".into())));
        assert_eq!(key_spec("7"), ("7".into(), "Digit7".into(), 55, Some("7".into())));
        assert_eq!(key_spec("F5"), ("F5".into(), "F5".into(), 116, None));
        assert_eq!(key_spec("escape").0, "Escape");
        assert_eq!(key_spec("space").1, "Space");
        // punctuation types itself even without a physical code
        assert_eq!(key_spec("/").3, Some("/".into()));
    }

    #[test]
    fn request_mistakes_are_not_navigation_artifacts() {
        assert!(clean_failure("no element with index 999 (3 found); call elements again"));
        assert!(clean_failure("wait: text not found within 300ms: nope"));
        assert!(clean_failure("select failed: the <select> has no option \"z\""));
        assert!(clean_failure("{\"blocked\":true,\"reason\":\"obscured\"}"));
        assert!(clean_failure(
            "'Publish' was already clicked in this run. Verify the result first"
        ));
        assert!(!clean_failure("Object reference chain is too long"));
        assert!(!clean_failure("the page closed the connection"));
    }

    #[test]
    fn checking_the_guard_without_recording_it_leaves_a_retry_possible() {
        let tab = format!("tab-{}", std::process::id());
        check_irreversible(&tab, "Publish", 7, false).unwrap();
        // Not recorded, so a second check still passes (a failed dispatch must not
        // consume the guard).
        check_irreversible(&tab, "Publish", 7, false).unwrap();
        record_irreversible(&tab, "Publish", 7);
        assert!(check_irreversible(&tab, "Publish", 7, false).is_err());
        // force still lifts it.
        check_irreversible(&tab, "Publish", 7, true).unwrap();
    }

    #[test]
    fn dismiss_matches_whole_words_only() {
        for (l, w) in [("ok", "ok"), ("ok, got it", "got it"), ("reject all", "reject"), ("×", "×"),
                       ("only necessary", "necessary")] {
            assert!(has_words(l, w), "{l} / {w}");
        }
        for (l, w) in [("book now", "ok"), ("facebook", "ok"), ("unlock", "ok"), ("closet", "close")] {
            assert!(!has_words(l, w), "{l} / {w}");
        }
    }

    #[test]
    fn irreversible_matches_final_confirmations_only() {
        for l in ["Post", "publish", "Share now", "Send", "Place order", "Delete post"] {
            assert!(irreversible(l), "{l}");
        }
        for l in ["Share", "Postcode", "Sender", "Settings"] {
            assert!(!irreversible(l), "{l}");
        }
    }

    #[test]
    fn modifier_names_map_to_cdp_bits() {
        assert_eq!(mod_mask(Some(&json!(["Shift", "ctrl"]))), 8 | 2);
        assert_eq!(mod_mask(Some(&json!(["Alt", "Meta"]))), 1 | 4);
        assert_eq!(mod_mask(None), 0);
    }

    #[test]
    fn a_multiple_selection_compares_as_a_set() {
        let got = json!(["c", "a"]);
        assert!(selection_matches(&["a".into(), "c".into()], &got));
        assert!(selection_matches(&["c".into(), "a".into(), "c".into()], &got));
        assert!(!selection_matches(&["a".into(), "b".into()], &got));
        assert!(!selection_matches(&[], &got));
        // A page that reports a string where an array was asked for is not a match.
        assert!(!selection_matches(&["a".into()], &json!("a")));
    }

    #[test]
    fn select_values_accepts_one_or_many() {
        assert_eq!(select_values(Some(&json!("pro"))).unwrap(), json!("pro"));
        assert_eq!(select_values(Some(&json!(["a", "c"]))).unwrap(), json!(["a", "c"]));
        assert!(select_values(Some(&json!([]))).unwrap_err().contains("at least one"));
        assert!(select_values(Some(&json!(5))).unwrap_err().contains("string or an array"));
        assert!(select_values(Some(&json!([1]))).unwrap_err().contains("must be strings"));
        assert!(select_values(None).unwrap_err().contains("needs value"));
    }

}
