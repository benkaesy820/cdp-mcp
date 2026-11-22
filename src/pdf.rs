// PDF export: one tool, one CDP command, and paper arithmetic that is tested
// here rather than discovered on a printed page.

use serde_json::{json, Value};

use crate::cdp::Conn;
use crate::{b64decode, data_dir};

#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct Paper {
    pub width: f64,
    pub height: f64,
}

#[derive(Debug)]
pub(crate) struct Plan {
    pub paper: Paper,
    pub format: &'static str,
    pub landscape: bool,
    pub print_background: bool,
    pub scale: f64,
    pub page_ranges: Option<String>,
    pub prefer_css_page_size: bool,
}

fn boolean(args: &Value, key: &str) -> Result<Option<bool>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(format!("{key} must be true or false")),
    }
}

fn number(args: &Value, key: &str) -> Result<Option<f64>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_f64()
            .filter(|f| f.is_finite())
            .map(Some)
            .ok_or_else(|| format!("{key} must be a number")),
    }
}

/// `"1-2,4"` is what the browser accepts; a typo should be a clear error here
/// rather than a protocol error after the page has been laid out.
fn page_ranges(raw: &str) -> Result<String, String> {
    let t = raw.trim();
    if t.is_empty()
        || !t
            .chars()
            .all(|c| c.is_ascii_digit() || c == ',' || c == '-' || c == ' ')
    {
        return Err("pageRanges must look like \"1-2,4\"".into());
    }
    Ok(t.to_string())
}

/// Decide the print parameters. Pure, so every rule is unit-tested.
pub(crate) fn plan(args: &Value) -> Result<Plan, String> {
    let format = match args.get("format") {
        None | Some(Value::Null) => "letter",
        Some(Value::String(s)) => match s.as_str() {
            "letter" => "letter",
            "a4" => "a4",
            other => return Err(format!("format must be letter or a4, not {other:?}")),
        },
        Some(_) => return Err("format must be letter or a4".into()),
    };
    let paper = match format {
        "a4" => Paper { width: 8.27, height: 11.69 },
        _ => Paper { width: 8.5, height: 11.0 },
    };
    let scale = number(args, "scale")?.unwrap_or(1.0);
    if !(0.1..=2.0).contains(&scale) {
        return Err("scale must be between 0.1 and 2.0".into());
    }
    let ranges = match args.get("pageRanges") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(page_ranges(s)?),
        Some(_) => return Err("pageRanges must be a string like \"1-2,4\"".into()),
    };
    Ok(Plan {
        paper,
        format,
        landscape: boolean(args, "landscape")?.unwrap_or(false),
        // Styled pages are what a human expects; stripped backgrounds are the
        // exception, so they are opt-out rather than opt-in.
        print_background: boolean(args, "printBackground")?.unwrap_or(true),
        scale,
        page_ranges: ranges,
        prefer_css_page_size: boolean(args, "preferCSSPageSize")?.unwrap_or(false),
    })
}

pub(crate) fn run(conn: &mut Conn, tab: &str, args: &Value) -> Result<Value, String> {
    let plan = plan(args)?;
    let mut params = json!({
        "landscape": plan.landscape,
        "printBackground": plan.print_background,
        "scale": plan.scale,
        "paperWidth": plan.paper.width,
        "paperHeight": plan.paper.height,
        "preferCSSPageSize": plan.prefer_css_page_size,
    });
    if let Some(r) = &plan.page_ranges {
        params["pageRanges"] = json!(r);
    }
    let r = conn.call("Page.printToPDF", params)?;
    if let Some(stream) = r.get("stream").and_then(Value::as_str) {
        // Only returned when a caller asked for transferMode:Stream, which this
        // tool never does.
        return Err(format!("printToPDF returned a stream handle, not bytes: {stream}"));
    }
    let data = r.get("data").and_then(Value::as_str).ok_or("printToPDF returned no data")?;
    let bytes = b64decode(data)
        .map_err(|e| format!("printToPDF returned invalid base64: {e}"))?;
    if !bytes.starts_with(b"%PDF-") {
        return Err(format!("printToPDF did not return a PDF: {} bytes", bytes.len()));
    }

    let path = match args.get("path").and_then(Value::as_str).filter(|p| !p.trim().is_empty()) {
        Some(p) => std::path::PathBuf::from(p),
        None => {
            let dir = data_dir().join("pdfs");
            std::fs::create_dir_all(&dir)
                .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
            let ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            dir.join(format!("page-{ms}.pdf"))
        }
    };
    std::fs::write(&path, &bytes).map_err(|e| format!("cannot write {}: {e}", path.display()))?;

    let mut out = json!({
        "tab": tab,
        "path": path.display().to_string(),
        "bytes": bytes.len(),
        "format": plan.format,
        "paper": { "width": plan.paper.width, "height": plan.paper.height },
        "landscape": plan.landscape,
        "printBackground": plan.print_background,
        "scale": plan.scale,
        "preferCSSPageSize": plan.prefer_css_page_size,
    });
    if let Some(r) = plan.page_ranges {
        out["pageRanges"] = json!(r);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paper_sizes_match_the_names() {
        let letter = plan(&json!({})).unwrap();
        assert_eq!(letter.format, "letter");
        assert_eq!((letter.paper.width, letter.paper.height), (8.5, 11.0));

        let a4 = plan(&json!({ "format": "a4" })).unwrap();
        assert_eq!((a4.paper.width, a4.paper.height), (8.27, 11.69));
    }

    #[test]
    fn defaults_favour_a_readable_page() {
        let p = plan(&json!({})).unwrap();
        assert!(!p.landscape);
        assert!(p.print_background, "backgrounds are printed unless told not to");
        assert_eq!(p.scale, 1.0);
        assert!(p.page_ranges.is_none());
        assert!(!p.prefer_css_page_size);
    }

    #[test]
    fn explicit_options_are_carried() {
        let p = plan(&json!({
            "format": "a4", "landscape": true, "printBackground": false,
            "scale": 0.75, "pageRanges": " 1-2, 4 ", "preferCSSPageSize": true
        }))
        .unwrap();
        assert!(p.landscape && !p.print_background && p.prefer_css_page_size);
        assert_eq!(p.scale, 0.75);
        assert_eq!(p.page_ranges.as_deref(), Some("1-2, 4"));
    }

    #[test]
    fn bad_parameters_are_rejected_with_a_reason() {
        let err = |v: Value| plan(&v).unwrap_err();
        assert!(err(json!({ "format": "legal" })).contains("letter or a4"));
        assert!(err(json!({ "scale": 0.05 })).contains("between 0.1 and 2.0"));
        assert!(err(json!({ "scale": 2.5 })).contains("between 0.1 and 2.0"));
        assert!(err(json!({ "pageRanges": "one" })).contains("1-2,4"));
        assert!(err(json!({ "pageRanges": "" })).contains("1-2,4"));
        assert!(err(json!({ "landscape": "yes" })).contains("true or false"));
    }
}
