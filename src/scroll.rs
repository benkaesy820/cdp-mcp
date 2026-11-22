// Wheel scrolling: a real `Input.dispatchMouseEvent` wheel at a point, and a
// settle that is measured from this side.
//
// The settle cannot live in the page. A tab that is not the one on screen
// suspends requestAnimationFrame and runs timers at about 1 Hz, so an in-page
// promise both answers late and hides whether the wheel arrived at all.

use serde_json::{json, Value};

use crate::cdp::Conn;

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

/// Which scroller moved: the page, the pane under the pointer, or neither.
/// A canvas answers "neither" while still having received the wheel.
fn moved(before: &Value, after: &Value) -> (bool, bool) {
    let page = (num(after, "y") - num(before, "y")).abs() > 1.0
        || (num(after, "x") - num(before, "x")).abs() > 1.0;
    let container = after.get("cy").and_then(Value::as_f64).is_some()
        && before.get("cy").and_then(Value::as_f64).is_some()
        && ((num(after, "cy") - num(before, "cy")).abs() > 1.0
            || (num(after, "cx") - num(before, "cx")).abs() > 1.0);
    (page, container)
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
    let token = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let js = |op: &str| format!("{WHEEL_JS}({op:?}, {x}, {y}, {token})");
    let begin = conn.eval(&js("begin"))?;
    for attempt in 0..2 {
        // A human moves the pointer first; Chromium latches the scroll target
        // to what is under it, so do the same.
        conn.call_many(&[
            ("Input.dispatchMouseEvent", json!({ "type": "mouseMoved", "x": x, "y": y })),
            ("Input.dispatchMouseEvent", json!({
                "type": "mouseWheel", "x": x, "y": y, "deltaX": dx, "deltaY": dy,
            })),
        ])?;
        std::thread::sleep(std::time::Duration::from_millis(60));
        let mut seen = conn.eval(&js("read"))?;
        if !reached_page(&seen) && attempt == 0 {
            // One slower look before sending it again: re-sending a wheel that
            // was merely late would scroll the page twice.
            std::thread::sleep(std::time::Duration::from_millis(120));
            seen = conn.eval(&js("read"))?;
        }
        if !reached_page(&seen) {
            // Dropped before it reached the page: one retry is honest, a silent
            // moved:false is not.
            continue;
        }
        if moved(&begin, &seen) == (false, false) {
            // A compositor-driven scroll may land a beat later; without this the
            // canvas answer and a slow scroll look alike.
            std::thread::sleep(std::time::Duration::from_millis(80));
        }
        break;
    }
    // `end` removes the listener and returns the settled state, so the page is
    // left as it was found even when the wheel was dropped.
    let after = conn.eval(&js("end"))?;
    let delivered = reached_page(&after);
    let (page_moved, cont_moved) = moved(&begin, &after);
    let (from, to, at_bottom) = if page_moved {
        (
            num(&begin, "y"),
            num(&after, "y"),
            num(&after, "y") + num(&after, "vh") + 2.0 >= num(&after, "h"),
        )
    } else if cont_moved {
        (
            num(&begin, "cy"),
            num(&after, "cy"),
            num(&after, "cy") + num(&after, "cvh") + 2.0 >= num(&after, "ch"),
        )
    } else {
        (num(&begin, "y"), num(&after, "y"), false)
    };
    // `moved:false` with `delivered:true` is a wheel that reached the page and
    // found nothing scrollable: a canvas got its event. It is not the same
    // answer as a wheel Chromium never delivered.
    Ok(json!({
        "from": from, "to": to, "moved": page_moved || cont_moved,
        "atBottom": at_bottom, "container": cont_moved, "delivered": delivered,
        "wheel": { "x": x, "y": y, "dx": dx, "dy": dy },
    }))
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

    #[test]
    fn movement_is_read_per_scroller() {
        let page = |y: f64, cy: Option<f64>| {
            json!({ "y": y, "x": 0.0, "h": 3000.0, "vh": 700.0,
                    "cy": cy, "cx": cy.map(|_| 0.0), "ch": 1200.0, "cvh": 200.0 })
        };
        assert_eq!(moved(&page(0.0, None), &page(400.0, None)), (true, false));
        assert_eq!(moved(&page(0.0, Some(0.0)), &page(0.0, Some(300.0))), (false, true));
        // A pixel of rounding, not a move.
        assert_eq!(moved(&page(100.0, None), &page(100.4, None)), (false, false));
        // No container at all (a canvas): neither scroller moved.
        assert_eq!(moved(&page(0.0, None), &page(0.0, None)), (false, false));
    }
}
