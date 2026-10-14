//! Entries: apps.txt descriptions + frecency history + full PATH scan
//! + Homebrew descriptions cache (24h). Mirrors pal_smart sections.

use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;

const DEFAULT_APPS: &str = r#"# pal_smart — interactive terminal applications
# Format: command → description

# [Agents]
pi → AI agent de código
prime-agent → AI agent primario
opencode → AI coding agent

# [Development]
nvim → Neovim editor
lazygit → Git interface

# [System]
mo → Mole: cleanup and optimization
"#;

fn trim(s: &str) -> String {
    s.trim().to_string()
}

/// cmd → description from apps.txt (decor only, like the zsh).
fn load_descs(config: &Path) -> HashMap<String, String> {
    let mut desc = HashMap::new();
    let Ok(text) = fs::read_to_string(config) else {
        return desc;
    };
    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() || !line.contains('→') {
            continue;
        }
        let mut parts = line.splitn(2, '→');
        let cmd = trim(parts.next().unwrap_or(""));
        let d = trim(parts.next().unwrap_or(""));
        if cmd.is_empty() || cmd.starts_with('[') {
            continue;
        }
        desc.entry(cmd).or_insert(d);
    }
    desc
}

fn ensure_apps_txt(dir: &Path) -> PathBuf {
    let p = dir.join("apps.txt");
    if !p.exists() {
        let _ = fs::write(&p, DEFAULT_APPS);
    }
    p
}

/// Homebrew descriptions cache (key "cmd: desc"), refreshed after 24h.
/// Missing cache refreshes synchronously; stale refreshes detached.
fn brew_descs(dir: &Path, desc: &mut HashMap<String, String>) {
    let cache = dir.join("brew-desc.txt");
    let stale = fs::metadata(&cache)
        .and_then(|m| m.modified())
        .map(|t| SystemTime::now().duration_since(t).unwrap_or(Duration::ZERO) > Duration::from_secs(86400))
        .unwrap_or(true);
    let missing = !cache.exists() || fs::metadata(&cache).map(|m| m.len() == 0).unwrap_or(true);
    if missing {
        refresh_brew_cache(&cache);
    } else if stale {
        let c = cache.clone();
        std::thread::spawn(move || refresh_brew_cache(&c));
    }
    let Ok(text) = fs::read_to_string(&cache) else {
        return;
    };
    for line in text.lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let (k, v) = (trim(k), trim(v.trim_start_matches(' ')));
        if k.is_empty() || v.is_empty() || desc.contains_key(&k) {
            continue;
        }
        desc.insert(k, v);
    }
}

fn refresh_brew_cache(cache: &Path) {
    let leaves = Command::new("brew").arg("leaves").arg("-r").output();
    let Ok(leaves) = leaves else { return };
    let text = String::from_utf8_lossy(&leaves.stdout).into_owned();
    let names: Vec<&str> = text.lines().collect();
    if names.is_empty() {
        return;
    }
    let mut cmd = Command::new("brew");
    cmd.arg("desc");
    for n in names {
        cmd.arg(n.trim());
    }
    if let Ok(out) = cmd.output() {
        let tmp = cache.with_extension("tmp");
        if fs::write(&tmp, &out.stdout).is_ok() {
            let _ = fs::rename(&tmp, cache);
        }
    }
}

// Cached PATH scan with mtime-based invalidation.
static PATH_SCAN_CACHE: OnceLock<Mutex<Option<(Vec<String>, SystemTime)>>> = OnceLock::new();

fn history_path(dir: &Path) -> PathBuf {
    dir.join("history")
}

fn scan_path_cached() -> Vec<String> {
    let cache = PATH_SCAN_CACHE.get_or_init(|| Mutex::new(None));
    let mut guard = cache.lock().unwrap();
    let now = SystemTime::now();
    // Re-scan if cache older than 5 minutes or empty.
    if let Some((ref cmds, ref ts)) = *guard {
        if now.duration_since(*ts).unwrap_or(Duration::MAX) < Duration::from_secs(300) {
            return cmds.clone();
        }
    }
    // Scan PATH directories.
    let mut cmds = Vec::new();
    let mut seen = HashSet::new();
    if let Ok(path_var) = env::var("PATH") {
        for d in path_var.split(':') {
            if let Ok(rd) = fs::read_dir(d) {
                for e in rd.flatten() {
                    let p = e.path();
                    if !is_executable(&p) { continue; }
                    if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                        if seen.insert(name.to_string()) {
                            cmds.push(name.to_string());
                        }
                    }
                }
            }
        }
    }
    cmds.sort();
    *guard = Some((cmds.clone(), now));
    cmds
}

/// Frecency top-20 from history (count desc), like the zsh pipeline.
fn frequent(dir: &Path) -> Vec<String> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    let mut order: Vec<String> = vec![];
    if let Ok(text) = fs::read_to_string(history_path(dir)) {
        for line in text.lines() {
            let cmd = trim(line.split_whitespace().next().unwrap_or("").split('→').next().unwrap_or(""));
            if cmd.is_empty() {
                continue;
            }
            if !counts.contains_key(&cmd) {
                order.push(cmd.clone());
            }
            *counts.entry(cmd).or_insert(0) += 1;
        }
    }
    let mut cmds = order;
    cmds.sort_by(|a, b| {
        counts[b].cmp(&counts[a]).then_with(|| b.cmp(a)) // sort -rn parity
    });
    cmds.into_iter().take(20).collect()
}

fn is_executable(p: &Path) -> bool {
    if !p.is_file() {
        return false; // dirs (e.g. *.mlmodelc bundles) have search +x but aren't commands
    }
    // access(X_OK) like shells do: real permission for THIS user,
    // not just "any exec bit set" (e.g. root-only cupsd is excluded).
    use std::ffi::CString;
    let Ok(c) = CString::new(p.as_os_str().as_encoded_bytes()) else {
        return false;
    };
    // SAFETY: c is a valid NUL-terminated path, access takes no ownership.
    unsafe { libc::access(c.as_ptr(), libc::X_OK) == 0 }
}

/// Append to history, keep last 200 lines.
pub fn bump_history(dir: &Path, cmd: &str) {
    let p = history_path(dir);
    let mut text = fs::read_to_string(&p).unwrap_or_default();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(cmd);
    text.push('\n');
    let lines: Vec<&str> = text.lines().collect();
    let keep = if lines.len() > 200 { &lines[lines.len() - 200..] } else { &lines[..] };
    let _ = fs::write(&p, keep.join("\n") + "\n");
}

fn label(cmd: &str, desc: &HashMap<String, String>, star: bool) -> String {
    let prefix = if star { "★ Frecuente | " } else { "PATH | " };
    match desc.get(cmd) {
        Some(d) => format!("{}{} → {}", prefix, cmd, d),
        None => format!("{}{}", prefix, cmd),
    }
}

/// Full entry list: frecency first (existing commands only), then PATH.
pub fn entries(dir: &Path) -> Vec<String> {
    let apps_txt = ensure_apps_txt(dir);
    let mut desc = load_descs(&apps_txt);
    // Load cached brew descriptions immediately (never blocks).
    load_brew_descs_cached(dir, &mut desc);

    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<String> = vec![];
    let mut freq_cmds: Vec<String> = vec![];

    for cmd in frequent(dir) {
        if seen.contains(&cmd) {
            continue;
        }
        // Use cached PATH scan to verify existence.
        if !scan_path_cached().contains(&cmd) {
            continue;
        }
        seen.insert(cmd.clone());
        freq_cmds.push(cmd.clone());
    }

    // Use cached PATH scan instead of re-scanning.
    let path_cmds: Vec<String> = scan_path_cached()
        .into_iter()
        .filter(|name| !seen.contains(name))
        .collect();

    // Schedule missing brew descs in background (never blocks UI).
    schedule_missing_brew_descs(dir, &freq_cmds, &path_cmds, &desc);

    for cmd in &freq_cmds {
        out.push(label(cmd, &desc, true));
    }
    for cmd in path_cmds {
        out.push(label(&cmd, &desc, false));
    }
    out
}

// Load brew descriptions from cache only (no network).
fn load_brew_descs_cached(dir: &Path, desc: &mut HashMap<String, String>) {
    let cache = dir.join("brew-desc.txt");
    let Ok(text) = fs::read_to_string(&cache) else { return; };
    for line in text.lines() {
        let Some((k, v)) = line.split_once(':') else { continue; };
        let (k, v) = (trim(k), trim(v.trim_start_matches(' ')));
        if k.is_empty() || v.is_empty() || desc.contains_key(&k) { continue; }
        desc.insert(k, v);
    }
}

// Schedule missing brew descs in background.
fn schedule_missing_brew_descs(dir: &Path, freq: &[String], path: &[String], existing: &HashMap<String, String>) {
    let missing: Vec<String> = freq.iter().chain(path.iter())
        .filter(|c| !existing.contains_key(*c))
        .cloned()
        .collect();
    if missing.is_empty() { return; }

    let cache = dir.join("brew-desc.txt");
    let missing_arc = Arc::new(missing);
    let cache_arc = Arc::new(cache);

    thread::spawn(move || {
        let mut cmd = Command::new("brew");
        cmd.arg("desc");
        for m in missing_arc.iter() { cmd.arg(m); }
        if let Ok(o) = cmd.output() {
            let text = String::from_utf8_lossy(&o.stdout).into_owned();
            let mut fresh = vec![];
            for line in text.lines() {
                if let Some((k, v)) = line.split_once(':') {
                    let (k, v) = (trim(k), trim(v.trim_start_matches(' ')));
                    if !k.is_empty() && !v.is_empty() {
                        fresh.push(line.to_string());
                    }
                }
            }
            if !fresh.is_empty() {
                let mut all = fs::read_to_string(&*cache_arc).unwrap_or_default();
                if !all.is_empty() && !all.ends_with('\n') { all.push('\n'); }
                all.push_str(&fresh.join("\n"));
                all.push('\n');
                let _ = fs::write(&*cache_arc, all);
            }
        }
    });
}
