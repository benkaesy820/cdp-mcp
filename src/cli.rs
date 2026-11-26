//! `cdp <verb>` — the same tools, one command at a time.
//!
//! Why a CLI exists at all: the MCP catalog is sent on every turn (17 KB), while
//! a shell command costs nothing to invoke. Microsoft's Playwright MCP README
//! says the same about its own product — CLI invocations "avoid loading large
//! tool schemas and verbose accessibility trees into the model context". So the
//! server stays for stateful loops, and this is the cheap path.
//!
//! Nothing here re-implements a tool: every verb builds the same arguments an
//! MCP call would and goes through `call_tool`, so behaviour cannot drift.

use serde_json::{json, Value};

use crate::Cfg;

pub(crate) const VERBS: [&str; 11] = [
    "status", "tabs", "go", "read", "find", "click", "type", "key", "shot", "eval", "json",
];

pub(crate) fn is_verb(s: &str) -> bool {
    VERBS.contains(&s)
}

/// The tab a verb acts on when `--tab` is not given: the first one the browser
/// lists, which is the one a person most recently used.
fn default_tab(cfg: &Cfg) -> Result<String, String> {
    let v = crate::call_tool(cfg, "status", &json!({}))?;
    v.get("tabs")
        .and_then(Value::as_array)
        .and_then(|t| t.first())
        .and_then(|t| t.get("tab"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "no page tab is open; `cdp tabs open URL` first".to_string())
}

/// Handles are positional in the CLI: each invocation is a new process, so the
/// server's handle table is empty. `t1` therefore means "the first tab `status`
/// lists", which is the same order the server uses when it assigns them.
fn resolve_tab(cfg: &Cfg, raw: &str) -> Result<String, String> {
    let Some(n) = raw.strip_prefix('t').and_then(|d| d.parse::<usize>().ok()) else {
        return Ok(raw.to_string());
    };
    let v = crate::call_tool(cfg, "status", &json!({}))?;
    v.get("tabs")
        .and_then(Value::as_array)
        .and_then(|t| t.get(n.wrapping_sub(1)))
        .and_then(|t| t.get("tab"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("no tab {raw}; run `cdp status` to see what exists"))
}

fn tab_lines(v: &Value) {
    let Some(tabs) = v.get("tabs").and_then(Value::as_array) else {
        // Not a listing: print the payload as it came.
        println!("{v}");
        return;
    };
    for t in tabs {
        // Padded spaces, not tabs: tabs collapse when a person copies a line out
        // of the terminal, and the columns then read as one word.
        println!(
            "{:<4} {:<44} {}",
            t.get("handle").and_then(Value::as_str).unwrap_or("-"),
            t.get("title").and_then(Value::as_str).unwrap_or("").chars().take(42).collect::<String>(),
            t.get("url").and_then(Value::as_str).unwrap_or("")
        );
    }
}

fn find_lines(v: &Value) {
    let Some(els) = v.get("elements").and_then(Value::as_array) else {
        println!("{v}");
        return;
    };
    for e in els {
        println!(
            "{:<4} {:<44} {:>5},{:<5} {}",
            e.get("i").and_then(Value::as_u64).unwrap_or(0),
            e.get("label").and_then(Value::as_str).unwrap_or("").chars().take(42).collect::<String>(),
            e.get("x").and_then(Value::as_i64).unwrap_or(0),
            e.get("y").and_then(Value::as_i64).unwrap_or(0),
            if e.get("role").and_then(Value::as_str) == Some("button") { "tap" } else { "---" }
        );
    }
    if let Some(hint) = v.get("hint").and_then(Value::as_str) {
        println!("# {hint}");
    }
}

pub(crate) fn run(cfg: &Cfg, argv: &[String]) -> i32 {
    let verb = argv.first().map(String::as_str).unwrap_or("status");
    let mut pos: Vec<String> = Vec::new();
    let mut tab_opt: Option<String> = None;
    let mut as_json = false;
    let mut max: Option<u64> = None;
    let mut path: Option<String> = None;
    let mut it = argv.iter().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--json" => as_json = true,
            "--tab" => tab_opt = it.next().cloned(),
            "--max" => max = it.next().and_then(|v| v.parse().ok()),
            "--path" => path = it.next().cloned(),
            other => pos.push(other.to_string()),
        }
    }
    // One place for the tab argument, so a handle is resolved exactly as the
    // server would resolve it.
    let with_tab = |mut args: Value| -> Result<Value, String> {
        let raw = match &tab_opt {
            Some(t) => t.clone(),
            None => default_tab(cfg)?,
        };
        args["tab"] = json!(resolve_tab(cfg, &raw)?);
        Ok(args)
    };
    let result: Result<Value, String> = (|| match verb {
        "status" => crate::call_tool(cfg, "status", &json!({})),
        "tabs" => match pos.first().map(String::as_str) {
            Some("open") => crate::call_tool(
                cfg,
                "tabs",
                &json!({ "action": "open", "url": pos.get(1).cloned().unwrap_or_default() }),
            ),
            Some("close") => crate::call_tool(
                cfg,
                "tabs",
                &json!({ "action": "close", "tab": pos.get(1).cloned().unwrap_or_default() }),
            ),
            _ => crate::call_tool(cfg, "tabs", &json!({ "action": "list" })),
        },
        "go" => {
            let url = pos.first().ok_or("go needs a URL")?;
            let mut args = with_tab(json!({ "action": "goto", "url": url }))?;
            args["action"] = json!("goto");
            args["url"] = json!(url);
            crate::call_tool(cfg, "tabs", &args)
        }
        "read" => {
            let mut args = with_tab(json!({}))?;
            if let Some(n) = max {
                args["limit"] = json!(n);
            }
            crate::call_tool(cfg, "text", &args)
        }
        "find" => {
            let q = pos.first().ok_or("find needs text")?;
            let mut args = with_tab(json!({ "limit": 30, "find": q, "full": true }))?;
            args["find"] = json!(q);
            crate::call_tool(cfg, "elements", &args)
        }
        "click" => {
            let i: u64 = pos.first().ok_or("click needs an element index")?.parse().map_err(|_| "click needs a number")?;
            let args = with_tab(json!({ "actions": [{ "click": { "i": i } }] }))?;
            crate::call_tool(cfg, "input", &args)
        }
        "type" => {
            let text = pos.first().ok_or("type needs text")?;
            let args = with_tab(json!({ "actions": [{ "insert": text }] }))?;
            crate::call_tool(cfg, "input", &args)
        }
        "key" => {
            let k = pos.first().ok_or("key needs a name (Enter, Escape, Tab)")?;
            let args = with_tab(json!({ "actions": [{ "key": k }] }))?;
            crate::call_tool(cfg, "input", &args)
        }
        "shot" => {
            let mut args = with_tab(json!({}))?;
            if let Some(p) = &path {
                args["path"] = json!(p);
            }
            crate::call_tool(cfg, "shot", &args)
        }
        "eval" => {
            let expr = pos.first().ok_or("eval needs an expression")?;
            let args = with_tab(json!({ "expression": expr }))?;
            crate::call_tool(cfg, "eval", &args)
        }
        "json" => {
            // Escape hatch: `cdp json elements '{"find":"Save"}'`.
            let tool = pos.first().ok_or("json needs a tool name")?;
            let args: Value = pos.get(1).map(|s| serde_json::from_str(s).unwrap_or(json!({}))).unwrap_or(json!({}));
            crate::call_tool(cfg, tool, &args)
        }
        other => Err(format!("unknown verb {other}; try status|tabs|go|read|find|click|type|key|shot|eval|json")),
    })();

    match result {
        Ok(v) => {
            // A doctor result is not a table of tabs: print the two lines a
            // person can act on, and fail the command so a script notices.
            let doctor = v.get("up").and_then(Value::as_bool) == Some(false);
            if as_json {
                println!("{v}");
            } else if doctor && verb == "status" {
                println!(
                    "no browser at {}",
                    v.get("endpoint").and_then(Value::as_str).unwrap_or("the default endpoint")
                );
                if let Some(e) = v.get("error").and_then(Value::as_str) {
                    println!("  {e}");
                }
                if let Some(hint) = v.get("hint").and_then(Value::as_str) {
                    println!("  {hint}");
                }
            } else if verb == "status" || verb == "tabs" {
                tab_lines(&v);
            } else if verb == "find" {
                find_lines(&v);
            } else if verb == "read" {
                println!("{}", v.get("text").and_then(Value::as_str).unwrap_or(""));
                if let Some(missing) = v.get("scopeMissing").and_then(Value::as_bool) {
                    if missing {
                        eprintln!("# scope matched nothing");
                    }
                }
            } else if verb == "shot" {
                println!("{}", v.get("path").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| v.to_string()));
            } else if verb == "eval" {
                println!("{}", v.get("value").map(|v| v.to_string()).unwrap_or_else(|| v.to_string()));
            } else {
                println!("{v}");
            }
            if doctor {
                1
            } else {
                0
            }
        }
        Err(e) => {
            eprintln!("ERROR: {e}");
            1
        }
    }
}
