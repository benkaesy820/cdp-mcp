// Viewport, touch and CSS media emulation.
//
// Two things make this more than a thin wrapper. `Emulation.setEmulatedMedia`
// replaces the entire feature list, so the active set is tracked per tab (see
// `Emulated`) and calls merge into it. And a viewport change moves every
// element, so the tab's scan cache is dropped rather than handed back with
// stale coordinates.

use serde_json::{json, Value};

use crate::cdp::{Conn, MediaState};
use crate::scan_cache;

const MIN_DIM: u64 = 100;
const MAX_DIM: u64 = 7680;

#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct Metrics {
    pub width: u32,
    pub height: u32,
    pub scale: f64,
    pub mobile: bool,
    pub touch: bool,
}

/// What one `emulate` call will do, decided without touching the browser.
#[derive(Default, Debug)]
pub(crate) struct Plan {
    pub reset: bool,
    pub metrics: Option<Metrics>,
    /// Touch changed on its own, with no viewport override.
    pub touch: Option<bool>,
    /// The complete media state after this call, when any media field was given.
    pub media: Option<MediaState>,
}

pub(crate) fn preset(name: &str) -> Option<Metrics> {
    match name {
        "mobile" => Some(Metrics { width: 390, height: 844, scale: 3.0, mobile: true, touch: true }),
        "tablet" => Some(Metrics { width: 768, height: 1024, scale: 2.0, mobile: true, touch: true }),
        "desktop" => Some(Metrics { width: 1280, height: 800, scale: 1.0, mobile: false, touch: false }),
        _ => None,
    }
}

fn int(args: &Value, key: &str) -> Result<Option<u64>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .map(Some)
            .ok_or_else(|| format!("{key} must be a whole number")),
    }
}

fn num(args: &Value, key: &str) -> Result<Option<f64>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_f64()
            .filter(|f| f.is_finite())
            .map(Some)
            .ok_or_else(|| format!("{key} must be a number")),
    }
}

fn boolean(args: &Value, key: &str) -> Result<Option<bool>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(format!("{key} must be true or false")),
    }
}

fn text<'a>(args: &'a Value, key: &str) -> Result<Option<&'a str>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(format!("{key} must be a string")),
    }
}

fn set_feature(features: &mut Vec<(String, String)>, name: &str, value: &str) {
    match features.iter_mut().find(|(n, _)| n == name) {
        Some(slot) => slot.1 = value.to_string(),
        None => features.push((name.to_string(), value.to_string())),
    }
}

/// Decide what a call does. Pure: the current media state is an argument, so
/// every rule here is unit-tested without a browser.
pub(crate) fn plan(args: &Value, current: &MediaState) -> Result<Plan, String> {
    let reset = boolean(args, "reset")?.unwrap_or(false);
    let preset_name = text(args, "preset")?;
    let base = match preset_name {
        Some(name) => Some(
            preset(name).ok_or_else(|| format!("unknown preset {name:?}; use mobile, tablet or desktop"))?,
        ),
        None => None,
    };
    let width = int(args, "width")?;
    let height = int(args, "height")?;
    let scale = num(args, "deviceScaleFactor")?;
    let mobile = boolean(args, "mobile")?;
    let touch = boolean(args, "touch")?;
    let color = text(args, "colorScheme")?;
    let motion = text(args, "reducedMotion")?;
    let media_kind = text(args, "media")?;

    let anything = reset
        || base.is_some()
        || width.is_some()
        || height.is_some()
        || scale.is_some()
        || mobile.is_some()
        || touch.is_some()
        || color.is_some()
        || motion.is_some()
        || media_kind.is_some();
    if !anything {
        return Err("emulate needs something to change: reset, preset, width/height, \
                    deviceScaleFactor, mobile, touch, colorScheme, reducedMotion or media"
            .into());
    }

    // Dimensions: with a preset an axis can be overridden on its own; without
    // one, both are required together.
    let wants_metrics =
        base.is_some() || width.is_some() || height.is_some() || scale.is_some() || mobile.is_some();
    if base.is_none() && mobile.is_some() && width.is_none() && height.is_none() {
        return Err("mobile needs a viewport: give width and height, or a preset".into());
    }
    let metrics = if wants_metrics {
        let w = width
            .or_else(|| base.map(|b| b.width as u64))
            .ok_or("width is required with height (or use a preset)")?;
        let h = height
            .or_else(|| base.map(|b| b.height as u64))
            .ok_or("height is required with width (or use a preset)")?;
        for (label, v) in [("width", w), ("height", h)] {
            if !(MIN_DIM..=MAX_DIM).contains(&v) {
                return Err(format!("{label} must be between {MIN_DIM} and {MAX_DIM}"));
            }
        }
        let scale = scale.or_else(|| base.map(|b| b.scale)).unwrap_or(1.0);
        if !(scale > 0.0) {
            return Err("deviceScaleFactor must be greater than 0".into());
        }
        let mobile = mobile.or_else(|| base.map(|b| b.mobile)).unwrap_or(false);
        // Touch follows mobile when the caller did not say otherwise.
        let touch = touch.unwrap_or(mobile);
        Some(Metrics { width: w as u32, height: h as u32, scale, mobile, touch })
    } else {
        None
    };
    let standalone_touch = if metrics.is_none() { touch } else { None };

    // Media merges into whatever is already active, and starts from empty after
    // a reset: setEmulatedMedia replaces the whole list.
    let mut media = if reset { MediaState::default() } else { current.clone() };
    let mut media_changed = false;
    if let Some(c) = color {
        if !matches!(c, "light" | "dark" | "no-preference") {
            return Err("colorScheme must be light, dark or no-preference".into());
        }
        if c == "no-preference" {
            // The CSS value was removed from the spec; dropping the override is
            // what "no preference" means, and it reverts to the browser default.
            media.features.retain(|(n, _)| n != "prefers-color-scheme");
        } else {
            set_feature(&mut media.features, "prefers-color-scheme", c);
        }
        media_changed = true;
    }
    if let Some(m) = motion {
        if !matches!(m, "reduce" | "no-preference") {
            return Err("reducedMotion must be reduce or no-preference".into());
        }
        set_feature(&mut media.features, "prefers-reduced-motion", m);
        media_changed = true;
    }
    if let Some(k) = media_kind {
        if !matches!(k, "screen" | "print" | "") {
            return Err("media must be screen, print or empty".into());
        }
        media.media = k.to_string();
        media_changed = true;
    }

    Ok(Plan {
        reset,
        metrics,
        touch: standalone_touch,
        media: media_changed.then_some(media),
    })
}

/// The page's own viewport, as the settle compares it.
fn viewport(conn: &mut Conn) -> Option<Value> {
    conn.eval("[innerWidth, innerHeight, Math.round(devicePixelRatio * 100)]")
        .ok()
}

/// A metrics override changes the renderer's layout on its next frame, not
/// inside the protocol reply: an immediate read still sees the old size, which
/// made `reset` look as though it had restored nothing. Wait for the change and
/// then for it to hold still, bounded.
fn settle_viewport(conn: &mut Conn, before: &Value) -> Result<(), String> {
    let Some(mut last) = viewport(conn) else { return Ok(()) };
    // Already different from the pre-call reading: the relayout happened before
    // the first look, so only stability is left to wait for.
    let mut changed = last != *before;
    for _ in 0..10 {
        crate::nap(30)?;
        let Some(now) = viewport(conn) else { return Ok(()) };
        if now != last {
            changed = true;
            last = now;
            continue;
        }
        if changed {
            break;
        }
    }
    Ok(())
}

/// What this tab is emulating. Kept per TAB, never on the connection: the
/// overrides live on the target in the browser, so they outlive a socket that
/// died, and the next `Emulation.setEmulatedMedia` replaces the whole feature
/// list. A connection-keyed record would silently drop a feature the page still
/// has.
#[derive(Clone, Debug, Default)]
pub(crate) struct Emulated {
    pub(crate) media: MediaState,
    /// Touch emulation is on. Tracked because disabling it only affects
    /// documents created afterwards: an existing page keeps `ontouchstart`.
    pub(crate) touch_on: bool,
}

/// Apply a call and report what is active, plus what the page itself measures.
pub(crate) fn run(
    conn: &mut Conn,
    tab: &str,
    args: &Value,
    state: &mut Emulated,
) -> Result<Value, String> {
    let plan = plan(args, &state.media)?;
    let viewport_before = if plan.reset || plan.metrics.is_some() {
        viewport(conn)
    } else {
        None
    };
    let mut applied = json!({ "tab": tab });
    // Touch events are bound when the document is created, so a change here only
    // reaches a page loaded afterwards. The caller is told rather than left to
    // discover it.
    let mut touch_needs_reload = false;

    // `maxTouchPoints` must be 1..16 even when disabling, so it is only sent
    // when touch is being turned on.
    let touch_param = |on: bool| {
        if on { json!({ "enabled": true, "maxTouchPoints": 1 }) } else { json!({ "enabled": false }) }
    };

    if plan.reset {
        conn.call("Emulation.clearDeviceMetricsOverride", json!({}))?;
        conn.call("Emulation.setTouchEmulationEnabled", touch_param(false))?;
        conn.call("Emulation.setEmulatedMedia", json!({ "media": "", "features": [] }))?;
        state.media = MediaState::default();
        if state.touch_on {
            touch_needs_reload = true;
        }
        state.touch_on = false;
        scan_cache().lock().unwrap().remove(tab);
        applied["reset"] = json!(true);
    }

    if let Some(m) = plan.metrics {
        conn.call(
            "Emulation.setDeviceMetricsOverride",
            json!({
                "width": m.width, "height": m.height,
                "deviceScaleFactor": m.scale, "mobile": m.mobile,
            }),
        )?;
        conn.call("Emulation.setTouchEmulationEnabled", touch_param(m.touch))?;
        if m.touch != state.touch_on {
            touch_needs_reload = true;
        }
        state.touch_on = m.touch;
        // Every coordinate and every `off` flag just moved; the scope did not.
        scan_cache().lock().unwrap().remove(tab);
        applied["viewport"] = json!({
            "width": m.width, "height": m.height,
            "deviceScaleFactor": m.scale, "mobile": m.mobile, "touch": m.touch,
        });
    } else if let Some(t) = plan.touch {
        conn.call("Emulation.setTouchEmulationEnabled", touch_param(t))?;
        if t != state.touch_on {
            touch_needs_reload = true;
        }
        state.touch_on = t;
        applied["touch"] = json!(t);
    }

    if let Some(media) = plan.media {
        let features: Vec<Value> = media
            .features
            .iter()
            .map(|(n, v)| json!({ "name": n, "value": v }))
            .collect();
        conn.call("Emulation.setEmulatedMedia", json!({ "media": media.media, "features": features }))?;
        let mut map = serde_json::Map::new();
        for (name, value) in &media.features {
            map.insert(name.clone(), json!(value));
        }
        applied["media"] = json!({ "media": media.media, "features": Value::Object(map) });
        state.media = media;
    }

    if let Some(b) = &viewport_before {
        if conn.dialog.is_none() {
            settle_viewport(conn, b)?;
        }
    }

    // The page's own answer. A dialog blocks the renderer, so ask only when it is
    // free; the applied values above are still true either way.
    if conn.dialog.is_none() {
        if let Ok(v) = conn.eval(
            "[innerWidth, innerHeight, devicePixelRatio, navigator.maxTouchPoints, 'ontouchstart' in window]",
        ) {
            if let Some(a) = v.as_array() {
                applied["measured"] = json!({
                    "width": a.first().and_then(Value::as_f64).unwrap_or(0.0),
                    "height": a.get(1).and_then(Value::as_f64).unwrap_or(0.0),
                    "dpr": a.get(2).and_then(Value::as_f64).unwrap_or(0.0),
                    "maxTouchPoints": a.get(3).and_then(Value::as_u64).unwrap_or(0),
                    "touchEvents": a.get(4).and_then(Value::as_bool).unwrap_or(false),
                });
            }
        }
    }
    if touch_needs_reload {
        applied["touchNeedsReload"] = json!(true);
        applied["note"] = json!(
            "touch events are created with the document: the page's 'ontouchstart' and \
             touch listeners only change after a reload (navigator.maxTouchPoints is \
             already updated)"
        );
    }
    if let Some(d) = conn.dialog.clone() {
        applied["dialog"] = d;
    }
    Ok(applied)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_preset_sets_every_axis() {
        let p = plan(&json!({ "preset": "mobile" }), &MediaState::default()).unwrap();
        let m = p.metrics.unwrap();
        assert_eq!((m.width, m.height, m.scale, m.mobile, m.touch), (390, 844, 3.0, true, true));

        let t = plan(&json!({ "preset": "tablet" }), &MediaState::default()).unwrap().metrics.unwrap();
        assert_eq!((t.width, t.height, t.scale, t.touch), (768, 1024, 2.0, true));

        let d = plan(&json!({ "preset": "desktop" }), &MediaState::default()).unwrap().metrics.unwrap();
        assert_eq!((d.width, d.height, d.scale, d.mobile, d.touch), (1280, 800, 1.0, false, false));
    }

    #[test]
    fn explicit_fields_override_the_preset() {
        let m = plan(
            &json!({ "preset": "mobile", "width": 400, "deviceScaleFactor": 2.5, "touch": false }),
            &MediaState::default(),
        )
        .unwrap()
        .metrics
        .unwrap();
        assert_eq!((m.width, m.height, m.scale, m.touch), (400, 844, 2.5, false));
    }

    #[test]
    fn dimensions_are_validated() {
        let err = |v: Value| plan(&v, &MediaState::default()).unwrap_err();
        assert!(err(json!({ "width": 390 })).contains("height is required"));
        assert!(err(json!({ "height": 844 })).contains("width is required"));
        assert!(err(json!({ "width": 99, "height": 844 })).contains("between 100 and 7680"));
        assert!(err(json!({ "width": 390, "height": 8000 })).contains("between 100 and 7680"));
        assert!(err(json!({ "width": 390, "height": 844, "deviceScaleFactor": 0 })).contains("greater than 0"));
        assert!(err(json!({ "mobile": true })).contains("needs a viewport"));
        assert!(err(json!({ "preset": "watch" })).contains("unknown preset"));
    }

    #[test]
    fn an_empty_call_is_rejected() {
        let e = plan(&json!({}), &MediaState::default()).unwrap_err();
        assert!(e.contains("needs something to change"), "{e}");
        let e = plan(&json!({ "tab": "ABC" }), &MediaState::default()).unwrap_err();
        assert!(e.contains("needs something to change"), "{e}");
    }

    #[test]
    fn media_features_merge_across_calls() {
        let first = plan(&json!({ "colorScheme": "dark" }), &MediaState::default()).unwrap();
        let media = first.media.unwrap();
        assert_eq!(media.features, vec![("prefers-color-scheme".to_string(), "dark".to_string())]);

        // The second call must carry the first feature forward, not replace it.
        let second = plan(&json!({ "reducedMotion": "reduce" }), &media).unwrap();
        let merged = second.media.unwrap();
        assert!(merged.features.contains(&("prefers-color-scheme".into(), "dark".into())));
        assert!(merged.features.contains(&("prefers-reduced-motion".into(), "reduce".into())));

        let back = plan(&json!({ "colorScheme": "light" }), &merged).unwrap().media.unwrap();
        assert!(back.features.contains(&("prefers-color-scheme".into(), "light".into())));
        assert!(back.features.contains(&("prefers-reduced-motion".into(), "reduce".into())));
    }

    #[test]
    fn media_kind_and_reset_were_considered() {
        let print = plan(&json!({ "media": "print" }), &MediaState::default()).unwrap().media.unwrap();
        assert_eq!(print.media, "print");

        // A reset starts from empty even when a media field comes with it.
        let dark = MediaState {
            media: String::new(),
            features: vec![("prefers-color-scheme".into(), "dark".into())],
        };
        let after = plan(&json!({ "reset": true, "reducedMotion": "reduce" }), &dark).unwrap();
        assert!(after.reset);
        let media = after.media.unwrap();
        assert!(!media.features.iter().any(|(n, _)| n == "prefers-color-scheme"));
        assert!(media.features.iter().any(|(n, _)| n == "prefers-reduced-motion"));

        // no-preference removes the override rather than sending the deprecated
        // CSS value.
        let back = plan(&json!({ "colorScheme": "no-preference" }), &dark).unwrap().media.unwrap();
        assert!(!back.features.iter().any(|(n, _)| n == "prefers-color-scheme"));

        let e = plan(&json!({ "colorScheme": "sepia" }), &MediaState::default()).unwrap_err();
        assert!(e.contains("colorScheme must be"));
        let e = plan(&json!({ "media": "projection" }), &MediaState::default()).unwrap_err();
        assert!(e.contains("media must be"));
    }

    #[test]
    fn touch_works_without_a_viewport() {
        let p = plan(&json!({ "touch": true }), &MediaState::default()).unwrap();
        assert!(p.metrics.is_none());
        assert_eq!(p.touch, Some(true));
    }

    #[test]
    fn a_viewport_alone_counts_as_a_change() {
        let p = plan(&json!({ "width": 390, "height": 844 }), &MediaState::default()).unwrap();
        let m = p.metrics.unwrap();
        // Touch follows mobile, and mobile defaults to false.
        assert!(!m.mobile && !m.touch);
        assert_eq!((m.width, m.height, m.scale), (390, 844, 1.0));
    }
}
