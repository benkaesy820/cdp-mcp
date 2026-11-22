// Screenshots: the capture path, the clip arithmetic, and the shot tool.

use serde_json::{json, Value};

use crate::cdp::{self, Conn};
use crate::scan::pick;
use crate::{b64decode, nonempty_scope, scope_for, with_conn, Cfg};

/// The MIME the bytes really are. `shot` used to report a WebP capture as JPEG.
pub(crate) fn mime_for(format: &str) -> &'static str {
    match format {
        "png" => "image/png",
        "webp" => "image/webp",
        _ => "image/jpeg",
    }
}

/// mode: "viewport" (what you see), "full" (whole document, height-capped) or
/// "custom" (a region in CSS pixels).
pub(crate) fn capture_png(
    conn: &mut cdp::Conn,
    mode: &str,
    region: Option<(f64, f64, f64, f64)>,
    max_w: Option<f64>,
    format: &str,
    quality: u32,
) -> Result<(Vec<u8>, bool, String), String> {
    let mut params = json!({ "format": format });
    if format == "jpeg" || format == "webp" {
        params["quality"] = json!(quality.min(100));
        // Trade a little file size for encode speed; a human glances at it.
        params["optimizeForSpeed"] = json!(true);
    }
    let mut clipped = false;

    match mode {
        "full" => {
            let metrics = conn.call("Page.getLayoutMetrics", json!({}))?;
            let size = metrics
                .get("cssContentSize")
                .or_else(|| metrics.get("contentSize"))
                .cloned()
                .unwrap_or(Value::Null);
            let w = size.get("width").and_then(Value::as_f64).unwrap_or(1280.0);
            let h_raw = size.get("height").and_then(Value::as_f64).unwrap_or(900.0);
            let h = h_raw.min(8000.0);
            clipped = h_raw > h;
            let scale = max_w.map(|m| (m / w).min(1.0)).unwrap_or(1.0);
            params["clip"] = json!({ "x": 0.0, "y": 0.0, "width": w, "height": h, "scale": scale });
            params["captureBeyondViewport"] = json!(true);
        }
        "custom" => {
            let (x, y, w, h) = region.ok_or("custom capture needs region {x,y,w,h}")?;
            let scale = max_w.map(|m| (m / w).min(1.0)).unwrap_or(1.0);
            params["clip"] = json!({ "x": x, "y": y, "width": w, "height": h, "scale": scale });
            params["captureBeyondViewport"] = json!(true);
        }
        _ => {
            if let Some(m) = max_w {
                let metrics = conn.call("Page.getLayoutMetrics", json!({}))?;
                let vp = metrics.get("cssVisualViewport").cloned().unwrap_or(Value::Null);
                let w = vp.get("clientWidth").and_then(Value::as_f64).unwrap_or(0.0);
                let h = vp.get("clientHeight").and_then(Value::as_f64).unwrap_or(0.0);
                let x = vp.get("pageX").and_then(Value::as_f64).unwrap_or(0.0);
                let y = vp.get("pageY").and_then(Value::as_f64).unwrap_or(0.0);
                if w > 0.0 && h > 0.0 && m < w {
                    params["clip"] = json!({ "x": x, "y": y, "width": w, "height": h, "scale": (m / w).min(1.0) });
                    params["captureBeyondViewport"] = json!(true);
                }
            }
        }
    }

    // A capture of a window that is not being rendered waits for a frame that
    // never comes. Bound it: ten seconds is already far past a working capture
    // (measured 98 ms on screen, 19 s off it), and the caller gets the same
    // "no frame" explanation the screencast guard gives instead of a socket
    // timeout.
    let r = match conn.call_within("Page.captureScreenshot", params, 10_000) {
        Ok(v) => v,
        Err(e) => {
            if conn.timed_out {
                conn.timed_out = false;
                return Err(format!(
                    "no frame: the browser window is not on the workspace you are viewing, \
                     so it is not being rendered. Switch to it and retry, or skip the \
                     screenshot — every other tool works without it. ({e})"
                ));
            }
            return Err(e);
        }
    };
    let b64 = r.get("data").and_then(Value::as_str).ok_or("screenshot returned no data")?;
    let mime = mime_for(format).to_string();
    Ok((b64decode(b64)?, clipped, mime))
}

/// Capture with the two things a background tab needs: frames while it runs, and
/// a human-opened tab's focus behaviour handed back afterwards. Both `shot` and
/// `elements { shot }` go through here, so the borrow/restore cannot drift.
pub(crate) fn capture_guarded(
    conn: &mut Conn,
    tab_id: &str,
    mode: &str,
    region: Option<(f64, f64, f64, f64)>,
    max_w: Option<f64>,
    fmt: &str,
    quality: u32,
) -> Result<(Vec<u8>, bool, String), String> {
    let borrowed = !cdp::is_owned(tab_id);
    if borrowed {
        conn.set_focus_emulation(true).ok();
    }
    // A capture needs a frame from the browser's compositor, and a window that
    // niri is not showing produces none: measured 12 s either way with
    // `fromSurface` true or false, against ~150 ms with the window on the visible
    // workspace. A live screencast forces frames while it runs, so the frame
    // arriving (or not) is also the honest answer to "can this be captured at
    // all".
    if let Err(why) = conn.start_frames() {
        if borrowed {
            let _ = conn.set_focus_emulation(false);
        }
        return Err(format!(
            "no frame: the browser window is not on the workspace you are viewing, \
             so it is not being rendered. Switch to it and retry, or skip the \
             screenshot — every other tool works without it. ({why})"
        ));
    }
    let shot = capture_png(conn, mode, region, max_w, fmt, quality);
    conn.stop_frames();
    if borrowed {
        let _ = conn.set_focus_emulation(false);
    }
    shot
}

/// The capture box for an element, widened by padding but never past the
/// document origin: CDP rejects a clip that starts before (0, 0).
pub(crate) fn padded_box(x: f64, y: f64, w: f64, h: f64, padding: f64) -> (f64, f64, f64, f64) {
    let px = (x - padding).max(0.0);
    let py = (y - padding).max(0.0);
    (px, py, (x + w + padding) - px, (y + h + padding) - py)
}

pub(crate) fn parse_region(v: Option<&Value>) -> Option<(f64, f64, f64, f64)> {
    let r = v?;
    Some((r.get("x")?.as_f64()?, r.get("y")?.as_f64()?, r.get("w")?.as_f64()?, r.get("h")?.as_f64()?))
}

/// Screenshot the tab: the viewport, the whole document, a region, or one element
/// by index. A capture borrows the tab through `capture_guarded`.

pub(crate) fn run(cfg: &Cfg, args: &Value) -> Result<Value, String> {
    let tab = args.get("tab").and_then(Value::as_str);
    let prof = cfg.profile_now();
    let fmt = args
        .get("format")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| prof.get_str("shot_format"))
        .unwrap_or_else(|| "png".into());
    let path = match args.get("path").and_then(Value::as_str) {
        Some(p) => p.to_string(),
        None => {
            // No path given: use the profile's folder and a unique name,
            // so an agent sets this once instead of inventing a name
            // for every capture.
            let dir = prof.shot_dir();
            std::fs::create_dir_all(&dir)
                .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
            let ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            dir.join(format!("shot-{ms}.{fmt}")).display().to_string()
        }
    };
    let mode = args.get("mode").and_then(Value::as_str).unwrap_or("viewport").to_string();
    let quality = args
        .get("quality")
        .and_then(Value::as_u64)
        .or_else(|| prof.get_u64("shot_quality"))
        .unwrap_or(85) as u32;
    let region = parse_region(args.get("region"));
    let index = args.get("i").and_then(Value::as_u64);
    // An explicit empty `within` means unscoped, as it does for elements;
    // only an absent one falls back to the tab's last scope.
    let within_given = matches!(args.get("within"), Some(v) if !v.is_null());
    let within = match args.get("within") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => nonempty_scope(s),
        Some(_) => return Err("shot: within must be a CSS selector".into()),
    };
    let padding = match args.get("padding") {
        None | Some(Value::Null) => 0.0,
        Some(v) => v
            .as_f64()
            .ok_or("shot: padding must be a number of pixels")?
            .clamp(0.0, 200.0),
    };
    if index.is_some() && region.is_some() {
        return Err("shot: i and region name two different captures; use one".into());
    }
    if index.is_some() && mode == "full" {
        return Err("shot: i cannot be combined with mode \"full\"".into());
    }
    with_conn(&cfg.endpoint, tab, |conn, tab_id| {
        // Resolve an element before anything is woken or borrowed: a
        // missing, detached or zero-size index must not leave a live
        // screencast and focus emulation behind, or spend 800 ms waking
        // the compositor just to fail.
        let (effective_mode, region, element) = match index {
            Some(i) => {
                let scope = if within_given { within.clone() } else { scope_for(tab_id) };
                let at = pick(conn, scope.as_deref(), i, true, None)?;
                if at.get("connected").and_then(Value::as_bool) == Some(false) {
                    return Err(format!("shot: element {i} is detached; call elements again"));
                }
                let rect = at
                    .get("rect")
                    .ok_or_else(|| format!("shot: the page did not report bounds for element {i}"))?;
                let (x, y, w, h) = (
                    rect.get("x").and_then(Value::as_f64).unwrap_or(0.0),
                    rect.get("y").and_then(Value::as_f64).unwrap_or(0.0),
                    rect.get("w").and_then(Value::as_f64).unwrap_or(0.0),
                    rect.get("h").and_then(Value::as_f64).unwrap_or(0.0),
                );
                if w < 1.0 || h < 1.0 {
                    return Err(format!("shot: element {i} has no visible box to capture"));
                }
                let (px, py, pw, ph) = padded_box(x, y, w, h, padding);
                let element = json!({
                    "i": i, "tag": at.get("tag"), "label": at.get("label"),
                    "x": px.round(), "y": py.round(), "w": pw.round(), "h": ph.round(),
                });
                ("custom".to_string(), Some((px, py, pw, ph)), Some(element))
            }
            None => (mode.clone(), region, None),
        };
        // Someone else's tab: let it render just long enough to be
        // captured, then hand its normal behaviour back.
        let (bytes, clipped, _mime) =
            capture_guarded(conn, tab_id, &effective_mode, region, None, &fmt, quality)?;
        std::fs::write(&path, &bytes).map_err(|e| format!("cannot write {path}: {e}"))?;
        let mut out = json!({
            "tab": tab_id, "path": path, "bytes": bytes.len(),
            // The CDP mode is "custom" for an element; the caller is
            // told what it actually captured.
            "mode": if index.is_some() { "element" } else { effective_mode.as_str() },
            "format": fmt, "clipped": clipped,
        });
        if let Some(el) = element {
            out["element"] = el;
        }
        Ok(out)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn element_padding_widens_without_crossing_the_origin() {
        assert_eq!(padded_box(100.0, 200.0, 160.0, 40.0, 20.0), (80.0, 180.0, 200.0, 80.0));
        assert_eq!(padded_box(10.0, 5.0, 160.0, 40.0, 20.0), (0.0, 0.0, 190.0, 65.0));
        assert_eq!(padded_box(0.0, 0.0, 160.0, 40.0, 0.0), (0.0, 0.0, 160.0, 40.0));
    }

    #[test]
    fn a_screenshot_reports_the_mime_it_really_is() {
        assert_eq!(mime_for("png"), "image/png");
        assert_eq!(mime_for("webp"), "image/webp");
        assert_eq!(mime_for("jpeg"), "image/jpeg");
        assert_eq!(mime_for("jpg"), "image/jpeg");
    }

}
