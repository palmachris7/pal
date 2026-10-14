//! Herdr integration: snapshot, split/tab open, popup close.
//! Mirrors pal_smart's --open flow + socket popup.close.

use serde_json::Value;
use std::env;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::time::Duration;
use std::{io::Write, thread};

fn herdr_bin() -> String {
    env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "/opt/homebrew/bin/herdr".to_string())
}

fn sock_path() -> String {
    env::var("HERDR_SOCKET_PATH").unwrap_or_else(|_| {
        format!(
            "{}/.config/herdr/herdr.sock",
            env::var("HOME").unwrap_or_default()
        )
    })
}

fn snapshot() -> Option<Value> {
    let out = Command::new(herdr_bin()).args(["api", "snapshot"]).output().ok()?;
    let v: Value = serde_json::from_slice(&out.stdout).ok()?;
    Some(snap_core(v))
}

/// Accepts {result:{snapshot}} or direct snapshot.
fn snap_core(v: Value) -> Value {
    if let Some(s) = v.get("result").and_then(|r| r.get("snapshot")) {
        return s.clone();
    }
    v
}

fn str_at(v: &Value, path: &[&str]) -> String {
    let mut cur = v;
    for p in path {
        match cur.get(*p) {
            Some(n) => cur = n,
            None => return String::new(),
        }
    }
    cur.as_str().unwrap_or("").to_string()
}

/// -> exit code for --open.
pub fn open_cmd(cmd: &str) -> i32 {
    let target_ws = env::var("PAL_TARGET_WS").unwrap_or_default();
    let active_pane = env::var("PAL_ACTIVE_PANE").unwrap_or_default();
    let mut tab_id = env::var("PAL_TAB_ID").unwrap_or_default();

    let snap = if target_ws.is_empty() || active_pane.is_empty() || tab_id.is_empty() {
        snapshot()
    } else {
        None
    };

    let mut target_ws = target_ws;
    let mut active_pane = active_pane;
    if let Some(ref s) = snap {
        if target_ws.is_empty() {
            if let Some(arr) = s.get("workspaces").and_then(|w| w.as_array()) {
                for w in arr {
                    if w.get("focused").and_then(|f| f.as_bool()).unwrap_or(false) {
                        target_ws = str_at(w, &["workspace_id"]);
                    }
                }
            }
        }
        if active_pane.is_empty() {
            active_pane = str_at(s, &["focused_pane_id"]);
        }
        if tab_id.is_empty() && !active_pane.is_empty() {
            if let Ok(out) = Command::new(herdr_bin())
                .args(["pane", "get", &active_pane])
                .output()
            {
                if let Ok(v) = serde_json::from_slice::<Value>(&out.stdout) {
                    let core = snap_core(v);
                    tab_id = str_at(&core, &["result", "pane", "tab_id"]);
                    if tab_id.is_empty() {
                        tab_id = str_at(&core, &["pane", "tab_id"]);
                    }
                }
            }
        }
        if tab_id.is_empty() && !target_ws.is_empty() {
            if let Some(tabs) = s.get("tabs").and_then(|t| t.as_array()) {
                for t in tabs {
                    let w = t.get("workspace_id").and_then(|x| x.as_str()).unwrap_or("");
                    let f = t.get("focused").and_then(|x| x.as_bool()).unwrap_or(false);
                    if w == target_ws && f {
                        tab_id = str_at(t, &["tab_id"]);
                    }
                }
            }
        }
    }
    if target_ws.is_empty() && active_pane.is_empty() {
        return 1;
    }

    // Pane count in target tab to determine split sequence:
    // 1 pane → split right (2)
    // 2 panes → split down (3)
    // 3 panes → split right (4)
    // 4+ panes → new tab
    let mut pane_count: usize = 0;
    if !tab_id.is_empty() {
        if let Ok(out) = Command::new(herdr_bin())
            .args(["pane", "list", "--workspace", &target_ws])
            .output()
        {
            if let Ok(v) = serde_json::from_slice::<Value>(&out.stdout) {
                let core = snap_core(v);
                let arr = core
                    .get("result")
                    .and_then(|r| r.get("panes"))
                    .or_else(|| core.get("panes"))
                    .and_then(|p| p.as_array());
                if let Some(arr) = arr {
                    pane_count = arr
                        .iter()
                        .filter(|p| p.get("tab_id").and_then(|t| t.as_str()).unwrap_or("") == tab_id)
                        .count();
                }
            }
        }
    }
    let mut mode = env::var("PAL_SMART_OPEN").unwrap_or("auto".into());
    if mode == "auto" {
        // Custom sequence: right, down, right, then tabs
        mode = match pane_count {
            0 | 1 => "split-right".to_string(),
            2 => "split-down".to_string(),
            3 => "split-right".to_string(),
            _ => "tab".to_string(),
        };
    }
    if mode.is_empty() {
        mode = "split-right".into();
    }

    let mut new_pane = String::new();
    if mode.starts_with("split") && !active_pane.is_empty() {
        let ok = Command::new(herdr_bin())
            .args(["pane", "get", &active_pane])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if ok {
            let direction = if mode == "split-down" { "down" } else { "right" };
            if let Ok(out) = Command::new(herdr_bin())
                .args([
                    "pane", "split", &active_pane,
                    "--direction", direction, "--ratio", "0.5", "--focus",
                ])
                .output()
            {
                if let Ok(v) = serde_json::from_slice::<Value>(&out.stdout) {
                    let core = snap_core(v);
                    new_pane = str_at(&core, &["result", "pane", "pane_id"]);
                    if new_pane.is_empty() {
                        new_pane = str_at(&core, &["pane", "pane_id"]);
                    }
                }
            }
        }
    }
    if new_pane.is_empty() {
        if let Ok(out) = Command::new(herdr_bin())
            .args(["tab", "create", "--workspace", &target_ws, "--label", cmd, "--focus"])
            .output()
        {
            if let Ok(v) = serde_json::from_slice::<Value>(&out.stdout) {
                let core = snap_core(v);
                new_pane = str_at(&core, &["result", "root_pane", "pane_id"]);
                if new_pane.is_empty() {
                    new_pane = str_at(&core, &["root_pane", "pane_id"]);
                }
            }
        }
    }
    if new_pane.is_empty() {
        return 1;
    }

    thread::sleep(Duration::from_millis(150));
    let _ = Command::new(herdr_bin())
        .args(["pane", "send-text", &new_pane, &format!("{}\n", cmd)])
        .output();
    0
}

/// Detached open (new session → survives popup close).
pub fn spawn_detached_open(cmd: &str, ws: &str, pane: &str, tab: &str, mode: &str, herdr_bin: &str) {
    let exe = env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or("pal".into());
    let mut c = Command::new(exe);
    c.arg("--open").arg(cmd);
    c.env("PAL_TARGET_WS", ws);
    c.env("PAL_ACTIVE_PANE", pane);
    c.env("PAL_TAB_ID", tab);
    c.env("PAL_SMART_OPEN", mode);
    c.env("HERDR_BIN_PATH", herdr_bin);
    c.stdin(std::process::Stdio::null());
    c.stdout(std::process::Stdio::null());
    c.stderr(std::process::Stdio::null());
    unsafe {
        c.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let _ = c.spawn();
}

/// Native popup.close over the Herdr socket (1s timeout).
pub fn popup_close() {
    let path = sock_path();
    let Ok(mut s) = UnixStream::connect(path) else {
        return;
    };
    let _ = s.set_read_timeout(Some(Duration::from_secs(1)));
    let _ = s.set_write_timeout(Some(Duration::from_secs(1)));
    let msg = r#"{"id":"pal-smart:popup-close","method":"popup.close","params":{}}"#.to_string() + "\n";
    if s.write_all(msg.as_bytes()).is_err() {
        return;
    }
    let mut buf = [0u8; 4096];
    use std::io::Read;
    let _ = s.read(&mut buf);
}
