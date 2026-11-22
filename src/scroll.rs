// Wheel scrolling: a real `Input.dispatchMouseEvent` wheel at a point, and a
// settle that is measured from this side.
//
// The settle cannot live in the page. A tab that is not the one on screen
// suspends requestAnimationFrame and runs timers at about 1 Hz, so an in-page
// promise both answers late and hides whether the wheel arrived at all.

use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{json, Value};

use crate::cdp::Conn;

/// Unique per call: the page keys its delivery listener on it. A counter, not a
/// clock, because `SystemTime::now().as_nanos()` is past 2^53 and the low digits
/// that make it unique are exactly the ones JavaScript drops.
static WHEEL_SEQ: AtomicU64 = AtomicU64::new(1);

/// One probe for a wheel: it counts the wheel events that actually reach the
/// page, and reports the scroll position of the page and of the pane under the
/// point. The pane is resolved once, at begin, so a page scroll that moves
/// other content under the pointer cannot swap in a different container
/// halfway through.
const WHEEL_JS: &str = r#"((op, x, y, token) => {
  const page = document.scrollingElement || document.documentElement;
  const key = '__cdp_wheel_' + token;
  const scrollable = (el) => {
    const st = getComputedStyle(el);
    const oy = st.overflowY, ox = st.overflowX;
    return ((oy === 'auto' || oy === 'scroll' || oy === 'overlay') && el.scrollHeight > el.clientHeight + 1)
        || ((ox === 'auto' || ox === 'scroll' || ox === 'overlay') && el.scrollWidth > el.clientWidth + 1);
  };
  const state = (c) => ({
    y: Math.round(scrollY), x: Math.round(scrollX), h: page.scrollHeight, vh: innerHeight,
    cy: c ? Math.round(c.scrollTop) : null, cx: c ? Math.round(c.scrollLeft) : null,
    ch: c ? c.scrollHeight : 0, cvh: c ? c.clientHeight : 0,
  });
  if (op === 'begin') {
    let el = document.elementFromPoint(x, y);
    for (; el && el !== document.body && el !== document.documentElement; el = el.parentElement) {
      if (scrollable(el)) break;
    }
    const c = (el && el !== document.body && el !== document.documentElement) ? el : null;
    const hit = { n: 0, c: c, listener: () => { hit.n++; } };
    window[key] = hit;
    addEventListener('wheel', hit.listener, { passive: true, capture: true });
    return state(c);
  }
  const hit = window[key] || { n: 0, c: null };
  const out = Object.assign({ n: hit.n }, state(hit.c));
  if (op === 'end') {
    removeEventListener('wheel', hit.listener, true);
    delete window[key];
  }
  return out;
})"#;

/// The wheel deltas. No deltas at all keeps the long-standing default (one
/// notch down); asking for no movement is a mistake worth naming.
pub(crate) fn deltas(dx: Option<f64>, dy: Option<f64>) -> Result<(f64, f64), String> {
    let (dx, dy) = match (dx, dy) {
        (None, None) => (0.0, 400.0),
        (dx, dy) => (dx.unwrap_or(0.0), dy.unwrap_or(0.0)),
    };
    if dx == 0.0 && dy == 0.0 {
        return Err("scroll needs a non-zero dy or dx".into());
    }
    Ok((dx, dy))
}

/// Whether the wheel event reached the page at all.
fn reached_page(seen: &Value) -> bool {
    seen.get("n").and_then(Value::as_u64).unwrap_or(0) > 0
}

fn num(v: &Value, k: &str) -> f64 {
    v.get(k).and_then(Value::as_f64).unwrap_or(0.0)
}

fn has_num(v: &Value, k: &str) -> bool {
    v.get(k).and_then(Value::as_f64).is_some()
}

/// What a wheel did, read from the two samples of the page.
#[derive(Debug, PartialEq)]
struct Wheel {
    from: f64,
    to: f64,
    /// The coordinate that moved, or the one the wheel asked for when nothing
    /// moved.
    axis: &'static str,
    moved: bool,
    /// The pane under the pointer is what this report is about.
    container: bool,
    /// The vertical scroller of record has reached its end. Reported even when
    /// nothing moved: a wheel at the bottom is `moved:false, atBottom:true`.
    at_bottom: bool,
}

/// Read a wheel's result. Pure, so the reporting rules are unit-tested instead
/// of inferred from whatever a page happens to do.
fn report(before: &Value, after: &Value, dx: f64, dy: f64) -> Wheel {
    let page_moved = (num(after, "y") - num(before, "y")).abs() > 1.0
        || (num(after, "x") - num(before, "x")).abs() > 1.0;
    let pane = has_num(before, "cy") && has_num(after, "cy");
    let pane_moved = pane
        && ((num(after, "cy") - num(before, "cy")).abs() > 1.0
            || (num(after, "cx") - num(before, "cx")).abs() > 1.0);
    // A page scroll does not make the pane its subject; a wheel that moved
    // nothing is still about the pane when there is one, because that is what it
    // was aimed at.
    let container = !page_moved && pane;
    let (fx, fy, tx, ty) = if container {
        (
            num(before, "cx"),
            num(before, "cy"),
            num(after, "cx"),
            num(after, "cy"),
        )
    } else {
        (num(before, "x"), num(before, "y"), num(after, "x"), num(after, "y"))
    };
    let (mx, my) = ((tx - fx).abs() > 1.0, (ty - fy).abs() > 1.0);
    let axis = if my && (!mx || (ty - fy).abs() >= (tx - fx).abs()) {
        "y"
    } else if mx {
        "x"
    } else if dy == 0.0 && dx != 0.0 {
        // Nothing moved, so report the direction that was asked for.
        "x"
    } else {
        "y"
    };
    let (from, to) = if axis == "x" { (fx, tx) } else { (fy, ty) };
    let (pos, size, view) = if container {
        (num(after, "cy"), num(after, "ch"), num(after, "cvh"))
    } else {
        (num(after, "y"), num(after, "h"), num(after, "vh"))
    };
    let at_bottom = size > 0.0 && pos + view + 2.0 >= size;
    Wheel {
        from,
        to,
        axis,
        moved: page_moved || pane_moved,
        container,
        at_bottom,
    }
}

/// Dispatch one wheel at (x, y) and report what it did.
///
/// A wheel is compositor work. In a tab that is not producing frames Chromium
/// defers the event by about a second and can drop it, so frame production is
/// requested for the duration: the same screencast `shot` uses. Unlike a
/// capture, the request alone is enough — waiting for its frames would add the
/// ~750 ms wake to every wheel, and the wheel's own ack is what proves the
/// compositor is running.
pub(crate) fn send(conn: &mut Conn, x: f64, y: f64, dx: f64, dy: f64) -> Result<Value, String> {
    let _ = conn.request_frames();
    let out = wheel(conn, x, y, dx, dy);
    conn.stop_frames();
    out
}

fn wheel(conn: &mut Conn, x: f64, y: f64, dx: f64, dy: f64) -> Result<Value, String> {
    let token = WHEEL_SEQ.fetch_add(1, Ordering::Relaxed);
    let js = |op: &str| format!("{WHEEL_JS}({op:?}, {x}, {y}, {token})");
    let begin = conn.eval(&js("begin"))?;
    if let Err(why) = dispatch(conn, &js, &begin, x, y, dx, dy) {
        // A failed call must not leave the delivery listener on the page. A dead
        // connection or an open dialog means there is nothing to clean up in.
        if !conn.dead && conn.dialog.is_none() {
            let _ = conn.eval_within(&js("end"), 400);
        }
        return Err(why);
    }
    // `end` removes the listener and returns the settled state, so the page is
    // left as it was found even when the wheel was dropped.
    let after = conn.eval(&js("end"))?;
    let delivered = reached_page(&after);
    let r = report(&begin, &after, dx, dy);
    // `moved:false` with `delivered:true` is a wheel that reached the page and
    // found nothing scrollable: a canvas got its event. It is not the same
    // answer as a wheel Chromium never delivered.
    Ok(json!({
        "from": r.from, "to": r.to, "axis": r.axis, "moved": r.moved,
        "atBottom": r.at_bottom, "container": r.container, "delivered": delivered,
        "wheel": { "x": x, "y": y, "dx": dx, "dy": dy },
    }))
}

/// Send the wheel and wait for the page to own it. A slower second look comes
/// before the retry: re-sending a wheel that was merely late would scroll twice.
fn dispatch(
    conn: &mut Conn,
    js: &dyn Fn(&str) -> String,
    begin: &Value,
    x: f64,
    y: f64,
    dx: f64,
    dy: f64,
) -> Result<(), String> {
    for attempt in 0..2 {
        // A human moves the pointer first; Chromium latches the scroll target to
        // what is under it, so do the same.
        conn.call_many(&[
            ("Input.dispatchMouseEvent", json!({ "type": "mouseMoved", "x": x, "y": y })),
            ("Input.dispatchMouseEvent", json!({
                "type": "mouseWheel", "x": x, "y": y, "deltaX": dx, "deltaY": dy,
            })),
        ])?;
        std::thread::sleep(std::time::Duration::from_millis(60));
        let mut seen = conn.eval(&js("read"))?;
        if !reached_page(&seen) && attempt == 0 {
            std::thread::sleep(std::time::Duration::from_millis(120));
            seen = conn.eval(&js("read"))?;
        }
        if !reached_page(&seen) {
            // Dropped before it reached the page: one retry is honest, a silent
            // moved:false is not.
            continue;
        }
        if !report(begin, &seen, dx, dy).moved {
            // A compositor-driven scroll may land a beat later; without this the
            // canvas answer and a slow scroll look alike.
            std::thread::sleep(std::time::Duration::from_millis(80));
        }
        break;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deltas_default_and_reject_a_no_op() {
        assert_eq!(deltas(None, None).unwrap(), (0.0, 400.0));
        assert_eq!(deltas(None, Some(-120.0)).unwrap(), (0.0, -120.0));
        assert_eq!(deltas(Some(30.0), None).unwrap(), (30.0, 0.0));
        assert!(deltas(Some(0.0), Some(0.0)).unwrap_err().contains("non-zero"));
    }

    fn sample(y: f64, x: f64, h: f64, vh: f64) -> Value {
        json!({ "y": y, "x": x, "h": h, "vh": vh, "cy": null, "cx": null,
                "ch": 0.0, "cvh": 0.0 })
    }

    fn pane(y: f64, cy: f64, ch: f64, cvh: f64) -> Value {
        json!({ "y": y, "x": 0.0, "h": 4000.0, "vh": 700.0,
                "cy": cy, "cx": 0.0, "ch": ch, "cvh": cvh })
    }

    #[test]
    fn a_wheel_at_the_bottom_reports_at_bottom() {
        let at = sample(1362.0, 0.0, 2080.0, 718.0);
        let r = report(&at, &at, 0.0, 400.0);
        assert!(!r.moved, "nothing moved");
        assert!(r.at_bottom, "but the page is at its end");
        assert_eq!((r.from, r.to, r.axis), (1362.0, 1362.0, "y"));
    }

    #[test]
    fn a_horizontal_wheel_reports_the_x_axis() {
        let r = report(
            &sample(0.0, 0.0, 500.0, 700.0),
            &sample(0.0, 300.0, 500.0, 700.0),
            300.0,
            0.0,
        );
        assert!(r.moved);
        assert_eq!((r.from, r.to, r.axis), (0.0, 300.0, "x"));
    }

    #[test]
    fn a_pane_is_the_subject_when_the_page_did_not_move() {
        let r = report(
            &pane(0.0, 0.0, 1200.0, 200.0),
            &pane(0.0, 300.0, 1200.0, 200.0),
            0.0,
            300.0,
        );
        assert!(r.moved && r.container && !r.at_bottom);
        assert_eq!((r.from, r.to), (0.0, 300.0));

        // At the pane's end the same wheel reports the bottom, not a silent no-op.
        let end = pane(0.0, 1010.0, 1200.0, 200.0);
        let r = report(&end, &end, 0.0, 300.0);
        assert!(!r.moved && r.container && r.at_bottom);
    }

    #[test]
    fn a_sub_pixel_change_is_not_a_move() {
        let r = report(
            &sample(100.0, 0.0, 3000.0, 700.0),
            &sample(100.4, 0.0, 3000.0, 700.0),
            0.0,
            400.0,
        );
        assert!(!r.moved && !r.at_bottom);
    }

}
