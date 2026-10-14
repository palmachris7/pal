//! pal — terminal app launcher (Rust port of pal_smart).
//! Curated zsh original: fzf menu + frecency + Herdr splits/tabs.
//! Modes match the original 1:1:
//!   pal                  → fzf popup menu, opens selection detached
//!   pal --list           → print entries (debug)
//!   pal --open <cmd>     → open <cmd> in split or new tab (internal)

mod apps;
mod herdr;
mod pick;

use std::env;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn config_dir() -> PathBuf {
    let base = env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(env::var("HOME").unwrap_or_default()).join(".config"));
    base.join("pal_smart")
}

/// Legacy fzf subprocess path (escape hatch PAL_USE_FZF=1).
fn fzf_run(fzf: &str, entries: &[String]) -> (String, String) {
    let input = entries.join("\n");
    let mut child = match Command::new(fzf)
        .args([
            "--height=100%",
            "--layout=reverse",
            "--border=rounded",
            "--info=hidden",
            "--prompt=  ⚡ ",
            "--header=Escribe para buscar o ejecutar (Enter = correr lo escrito)",
            "--scheme=history",
            "--print-query",
            "--delimiter=|",
            "--with-nth=2",
            "--color=fg:#81a1c1,bg:-1,hl:#88c0d0,fg+:white,bg+:black,hl+:white,prompt:#8fbcbb,border:#5e81ac",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => std::process::exit(0),
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(input.as_bytes());
    }
    let out = child.wait_with_output().map(|o| o.stdout).unwrap_or_default();
    let text = String::from_utf8_lossy(&out);
    let mut lines = text.lines();
    (
        lines.next().unwrap_or("").trim().to_string(),
        lines.next().unwrap_or("").trim().to_string(),
    )
}

fn main() {
    // Herdr popups may launch with a minimal launchd PATH (same as zsh).
    let extra = format!(
        "/opt/homebrew/bin:/etc/profiles/per-user/{}/bin:{}//.local/bin",
        env::var("USER").unwrap_or_default(),
        env::var("HOME").unwrap_or_default()
    );
    let path = format!("{}:{}", extra, env::var("PATH").unwrap_or_default());
    // SAFETY: single-threaded at startup, before any threads spawn.
    unsafe { env::set_var("PATH", path) };

    let args: Vec<String> = env::args().skip(1).collect();
    if args.len() >= 2 && args[0] == "--open" {
        std::process::exit(herdr::open_cmd(&args[1]));
    }
    if args.first().map(|s| s.as_str()) == Some("--version") {
        println!("pal {} ({})", env!("CARGO_PKG_VERSION"), env!("GIT_HASH"));
        return;
    }

    let dir = config_dir();
    let _ = fs::create_dir_all(&dir);
    let entries = apps::entries(&dir);

    if args.first().map(|s| s.as_str()) == Some("--list") {
        let mut out = String::new();
        for e in &entries {
            out.push_str(e);
            out.push('\n');
        }
        let _ = std::io::stdout().write_all(out.as_bytes());
        return;
    }

    let herdr_bin = env::var("HERDR_BIN_PATH").unwrap_or("/opt/homebrew/bin/herdr".into());
    if which(&herdr_bin).is_none() && !PathBuf::from(&herdr_bin).exists() {
        eprint!("pal_smart: Herdr binary not found: {}", herdr_bin);
        std::process::exit(127);
    }

    // Native picker (nucleo+Ratatui) unless PAL_USE_FZF=1 escape hatch.
    let (query, sel): (String, String) = if env::var("PAL_USE_FZF").as_deref() == Ok("1") {
        let fzf = match which("fzf") {
            Some(p) => p,
            None => {
                eprint!("pal_smart: fzf is required");
                std::process::exit(127);
            }
        };
        fzf_run(&fzf, &entries)
    } else {
        match pick::run(&entries) {
            Some((q, s)) => (q, s.unwrap_or_default()),
            None => return, // Esc: silent exit like fzf
        }
    };
    let query = query.trim().to_string();
    let sel = sel.trim().to_string();

    // sel format: "<tag>| <cmd> → <desc>" — same parse as zsh.
    let mut cmd = if !sel.is_empty() {
        let after = sel.split('|').nth(1).unwrap_or("").trim().to_string();
        after.split('→').next().unwrap_or("").trim().to_string()
    } else {
        query
    };
    cmd = cmd.trim().to_string();
    if cmd.is_empty() {
        return;
    }
    // Questions (? / ??) are answered inline by the picker (copy+dismiss).
    // Shell runs (>cmd) too. They never open a pane.
    if cmd.starts_with('?') || cmd.starts_with('>') {
        return;
    }

    apps::bump_history(&dir, &cmd);

    // Detached open (new session → survives popup close), like the zsh.
    let ws = env::var("PAL_TARGET_WS").unwrap_or_default();
    let pane = env::var("PAL_ACTIVE_PANE").unwrap_or_default();
    let tab = env::var("PAL_TAB_ID").unwrap_or_default();
    let mode = env::var("PAL_SMART_OPEN").unwrap_or("auto".into());
    herdr::spawn_detached_open(&cmd, &ws, &pane, &tab, &mode, &herdr_bin);
    herdr::popup_close();
}

fn which(name: &str) -> Option<String> {
    if name.contains('/') {
        let p = PathBuf::from(name);
        if p.exists() {
            return Some(name.to_string());
        }
        return None;
    }
    for dir in env::var("PATH").unwrap_or_default().split(':') {
        let p = PathBuf::from(dir).join(name);
        if p.is_file() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(md) = fs::metadata(&p) {
                    if md.permissions().mode() & 0o111 != 0 {
                        return Some(p.to_string_lossy().into_owned());
                    }
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    // Question dismissal (no pane) is structural: cmd starting with '?'
    // returns before spawn_detached_open. Covered by picker tests.
}
