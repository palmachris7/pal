//! Native fuzzy picker (nucleo + Ratatui): fzf parity without subprocess.
//! Same look (prompt ⚡, rounded border, nord-ish colors, reverse layout),
//! same semantics: Enter runs selection (or typed query), Esc aborts.

use crossterm::event::{self, Event, KeyCode, KeyModifiers, MouseButton, MouseEventKind};
use nucleo::{
    pattern::{CaseMatching, Normalization, Pattern},
    Config, Matcher, Utf32Str,
};
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};
use ratatui::{
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph},
    Terminal,
};
use ratatui::backend::CrosstermBackend;
use std::io;
use unicode_width::UnicodeWidthStr;

// Palette roles (one accent, nord soul):
// BORDER = quiet chrome, FG = content, PROMPT = input voice,
// HEADER = dim context, MARK = the single accent (gutter/thumb/spinner),
// HL = matched-query shimmer, White = selection climax.
const FG: Color = Color::Rgb(0x81, 0xa1, 0xc1);
const PROMPT: Color = Color::Rgb(0x8f, 0xbc, 0xbb);
const BORDER: Color = Color::Rgb(0x5e, 0x81, 0xac);
const HEADER: Color = Color::Indexed(109);
const MARK: Color = Color::Rgb(0x88, 0xC0, 0xD0); // nord8 frost: the accent
const HL: Color = Color::Rgb(0xEB, 0xCB, 0x8B); // nord13 aurora: match shimmer
const CODE: Color = Color::Indexed(180);

const SPIN: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠿"];

fn cell_w(s: &str) -> u16 {
    UnicodeWidthStr::width(s) as u16
}

/// Elegant truncation: cut by cell width, end with … (never mid-word abrupt).
fn fit(s: &str, max_w: usize) -> String {
    if UnicodeWidthStr::width(s) <= max_w {
        return s.to_string();
    }
    let mut w = 0;
    let mut out = String::new();
    for c in s.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(1);
        if w + cw + 1 > max_w {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('…');
    out
}

/// (display_text, char_offset_of_display_within_entry).
fn display_span(entry: &str) -> (String, usize) {
    let Some(pos) = entry.find('|') else {
        return (entry.to_string(), 0);
    };
    let pre: usize = entry[..pos].chars().count() + 1;
    let rest = &entry[pos + 1..];
    let lead: usize = rest.chars().take_while(|c| c.is_whitespace()).count();
    (rest.trim().to_string(), pre + lead)
}

/// Per-char highlight spans for matched indices (char positions in `text`).
fn hl_spans(text: &str, hits: &[u32], base: Style, hl: Style) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut cur: Option<(bool, String)> = None;
    for (i, c) in text.chars().enumerate() {
        let hit = hits.contains(&(i as u32));
        match &mut cur {
            Some((h, s)) if *h == hit => s.push(c),
            _ => {
                if let Some((h, s)) = cur.take() {
                    out.push(Span::styled(s, if h { hl } else { base }));
                }
                cur = Some((hit, c.to_string()));
            }
        }
    }
    if let Some((h, s)) = cur.take() {
        out.push(Span::styled(s, if h { hl } else { base }));
    }
    out
}

/// Minimal markdown: `# ` title, `**bold**`, `` `code` ``. Quiet, not clever.
fn md_spans(line: &str, base: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut rest = line;
    if let Some(t) = rest.strip_prefix("### ") {
        spans.push(Span::styled(format!("  {}", t), base.fg(Color::White).add_modifier(Modifier::BOLD)));
        return spans;
    }
    if let Some(t) = rest.strip_prefix("## ") {
        spans.push(Span::styled(format!("  {}", t), base.fg(Color::White).add_modifier(Modifier::BOLD)));
        return spans;
    }
    if let Some(t) = rest.strip_prefix("# ") {
        spans.push(Span::styled(format!("  {}", t), base.fg(Color::White).add_modifier(Modifier::BOLD)));
        return spans;
    }
    spans.push(Span::raw("  "));
    let bold = base.fg(Color::White).add_modifier(Modifier::BOLD);
    let code = base.fg(CODE);
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix("**") {
            if let Some(end) = after.find("**") {
                spans.push(Span::styled(after[..end].to_string(), bold));
                rest = &after[end + 2..];
                continue;
            }
        }
        if let Some(after) = rest.strip_prefix('`') {
            if let Some(end) = after.find('`') {
                spans.push(Span::styled(after[..end].to_string(), code));
                rest = &after[end + 1..];
                continue;
            }
        }
        // Emit up to next marker.
        let next_bb = rest.find("**").unwrap_or(rest.len());
        let next_bt = rest.find('`').unwrap_or(rest.len());
        let n = next_bb.min(next_bt).max(1);
        let (head, tail) = rest.split_at(n);
        spans.push(Span::styled(head.to_string(), base));
        rest = tail;
    }
    spans
}

/// Thumb metrics (single source of truth, fzf getScrollbar style).
/// Returns (start_row, len) within the visible window.
fn thumb(view_h: usize, total: usize, offset: usize) -> (usize, usize) {
    if total <= view_h {
        return (0, view_h);
    }
    let len = (view_h * view_h / total).max(1);
    let start = offset * (view_h - len) / (total - view_h).max(1);
    (start, len)
}

/// Inverse mapping: window row (+grab) -> list offset.
fn row_to_offset(r: usize, grab: usize, view_h: usize, th_len: usize, total: usize) -> usize {
    let span = total.saturating_sub(view_h);
    if span == 0 {
        return 0;
    }
    let denom = (view_h - th_len).max(1);
    (r.saturating_sub(grab) * span / denom).min(span)
}

/// Minimal self-diagnostic journal (numbers only, no content).
/// One line per STATE CHANGE + exit reason. For remote debugging.
struct Journal {
    path: std::path::PathBuf,
    last: String,
}

impl Journal {
    fn open() -> Self {
        // Fixed stable path (both user and debugger can find it).
        let path = std::path::PathBuf::from("/tmp/pal-journal.log");
        // Cap: restart file past 200KB.
        if std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) > 200_000 {
            let _ = std::fs::remove_file(&path);
        }
        Journal { path, last: String::new() }
    }

    fn line(&mut self, msg: String) {
        use std::io::Write as _;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&self.path) {
            let _ = writeln!(f, "{}", msg);
        }
    }

    fn state(&mut self, cursor: usize, offset: usize, n: usize, view: usize, qlen: usize, ans: bool, evn: u64) {
        let s = format!("c{} o{} n{} v{} q{} a{} e{}", cursor, offset, n, view, qlen, ans as u8, evn);
        if s != self.last {
            self.last = s.clone();
            self.line(s);
        }
    }
}

/// `?pregunta` / `??idea` → (opencode, texto). None = not a question.
// TODO: multi-agent config; por ahora todo es opencode.
fn is_question(q: &str) -> Option<(&'static str, String)> {
    let t = q.trim();
    if let Some(r) = t.strip_prefix("??") {
        let r = r.trim();
        if r.is_empty() {
            return None;
        }
        return Some(("opencode", r.to_string()));
    }
    if let Some(r) = t.strip_prefix('?') {
        let r = r.trim();
        if r.is_empty() {
            return None;
        }
        return Some(("opencode", r.to_string()));
    }
    None
}

/// `>comando` → shell inline. None = not a shell command.
fn is_shell(q: &str) -> Option<String> {
    let r = q.trim().strip_prefix('>')?.trim();
    if r.is_empty() {
        return None;
    }
    Some(r.to_string())
}

/// Conversation block marker: user questions/commands in the transcript.
const QBLOCK: &str = "❯ ";

/// Last assistant answer in the transcript (lines after the final ❯ block).
/// Empty while the current turn is still thinking.
fn last_answer(lines: &[String]) -> Vec<String> {
    let mut out = vec![];
    for l in lines.iter().rev() {
        if l.starts_with(QBLOCK) {
            break;
        }
        out.push(l.clone());
    }
    out.reverse();
    out
}

enum AnsEvt {
    Line(String),
    Done,
}

/// Parsed bits of one `opencode run --format json` JSONL event.
struct Parsed {
    session: Option<String>,
    texts: Vec<String>,
    error: Option<String>,
}

fn str_at(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(|s| s.to_string())
}

/// Tolerant parse: complete text blocks only (no deltas), skip tool/reasoning noise.
fn parse_json_line(line: &str) -> Parsed {
    let mut out = Parsed { session: None, texts: vec![], error: None };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { return out };
    if let Some(s) = str_at(&v, "sessionID") {
        if s.starts_with("ses_") {
            out.session = Some(s);
        }
    }
    let t = str_at(&v, "type").unwrap_or_default();
    match t.as_str() {
        "text" => {
            // Try known shapes: {text}, {part:{text}}, {properties:{part:{text}}}, {delta}.
            let mut found: Option<String> = str_at(&v, "text")
                .or_else(|| str_at(&v, "delta"))
                .or_else(|| v.get("part").and_then(|p| str_at(p, "text")))
                .or_else(|| {
                    v.get("properties")
                        .and_then(|p| p.get("part"))
                        .and_then(|p| str_at(p, "text"))
                });
            if found.is_none() {
                // Last resort: {data:{text}} / {message:{text}}.
                found = v
                    .get("data")
                    .and_then(|d| str_at(d, "text"))
                    .or_else(|| v.get("message").and_then(|m| str_at(m, "text")));
            }
            if let Some(text) = found {
                if !text.trim().is_empty() {
                    out.texts.push(text);
                }
            }
        }
        "error" => {
            let msg = v
                .get("error")
                .and_then(|e| str_at(e, "message").or_else(|| str_at(e, "text")))
                .or_else(|| str_at(&v, "message"))
                .unwrap_or_else(|| "error desconocido".into());
            out.error = Some(msg);
        }
        _ => {}
    }
    out
}

fn kill_answer(child: &mut Option<Child>) {
    if let Some(mut c) = child.take() {
        // Children run setsid'd in their own group: kill the whole tree
        // (no orphans), then reap. Harmless for already-dead children.
        #[cfg(unix)]
        unsafe {
            libc::killpg(c.id() as i32, libc::SIGKILL);
        }
        let _ = c.kill();
        let _ = c.wait();
    }
}

/// Enter on an answer: copy it to clipboard, dismiss with no pane.
fn copy_answer(lines: &[String]) {
    use std::io::Write as _;
    if lines.is_empty() {
        return;
    }
    if let Ok(mut c) = Command::new("pbcopy").stdin(Stdio::piped()).spawn() {
        if let Some(mut stdin) = c.stdin.take() {
            let _ = stdin.write_all(lines.join("\n").as_bytes());
        }
        // Wait for it: an orphaned pbcopy gets SIGHUP'd and drops the text.
        let _ = c.wait();
    }
}

fn reset_answer(child: &mut Option<Child>, active: &mut Option<u64>) {
    // Conversation transcripts survive edits: only stop the live run.
    // Scroll position and history are kept (reader-friendly).
    kill_answer(child);
    *active = None;
}

fn spawn_answer(
    agent: &str,
    prompt: &str,
    generation: u64,
    session: Option<String>,
    tx: std::sync::mpsc::Sender<(u64, AnsEvt)>,
) -> Option<Child> {
    // PAL_OCODE_BIN lets users pin the exact agent binary. Todo opencode por ahora.
    let _ = agent;
    let mut cmd = Command::new(std::env::var("PAL_OCODE_BIN").unwrap_or("opencode".into()));
    // Headless JSONL: one JSON object per line (step_start/text/tool_use/...).
    // --session continues the conversation within this pal run.
    if let Some(s) = session {
        cmd.args(["run", "-s", &s, "--format", "json", prompt]);
    } else {
        cmd.args(["run", "--format", "json", prompt]);
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::null()).stdin(Stdio::null());
    // Own process group so killpg() below only ever hits our subtree.
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt as _;
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = cmd.spawn().ok()?;
    let out = child.stdout.take()?;
    std::thread::spawn(move || {
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            if tx.send((generation, AnsEvt::Line(line))).is_err() {
                break;
            }
        }
        let _ = tx.send((generation, AnsEvt::Done));
    });
    Some(child)
}

/// Shell command inline (`>cmd`): stdout+stderr drained by two threads
/// into the same channel (approximate interleaving, fine for display),
/// own process group, exit code observed by the pump via try_wait.
///
/// Runs under YOUR shell (zsh -i): aliases and functions from ~/.zshrc
/// work like in a pane. PAL_SHELL overrides ("sh" for plain POSIX).
/// Filters zshrc init noise lines like `(eval):1: ...`.
fn spawn_shell(
    shell_cmd: &str,
    generation: u64,
    tx: std::sync::mpsc::Sender<(u64, AnsEvt)>,
) -> Option<Child> {
    let (bin, args): (String, Vec<&str>) = match std::env::var("PAL_SHELL") {
        Ok(s) => (s, vec!["-c"]),
        Err(_) => ("zsh".into(), vec!["-i", "-c"]),
    };
    let mut cmd = Command::new(bin);
    cmd.args(args).arg(shell_cmd);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).stdin(Stdio::null());
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt as _;
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = cmd.spawn().ok()?;
    let out = child.stdout.take()?;
    let err = child.stderr.take()?;
    pipe_lines(BufReader::new(out), generation, tx.clone());
    pipe_lines(BufReader::new(err), generation, tx);
    Some(child)
}

fn pipe_lines<R: BufRead + Send + 'static>(
    r: R,
    generation: u64,
    tx: std::sync::mpsc::Sender<(u64, AnsEvt)>,
) {
    std::thread::spawn(move || {
        for line in r.lines().map_while(Result::ok) {
            if tx.send((generation, AnsEvt::Line(line))).is_err() {
                break;
            }
        }
        // EOF on our stream. The pump finalizes only if the child
        // actually exited (try_wait); otherwise it keeps waiting.
        let _ = tx.send((generation, AnsEvt::Done));
    });
}

#[cfg(test)]
fn rank_pub(entries: &[String], query: &str) -> Vec<usize> {
    if query.is_empty() {
        return (0..entries.len()).collect();
    }
    rank_with(entries, &Pattern::parse(query, CaseMatching::Smart, Normalization::Smart))
}

fn rank_with(entries: &[String], pat: &Pattern) -> Vec<usize> {
    let mut matcher = Matcher::new(Config::DEFAULT);
    let mut buf = Vec::new();
    let mut scored: Vec<(u32, usize)> = vec![];
    for (i, e) in entries.iter().enumerate() {
        let hay = Utf32Str::new(e, &mut buf);
        if let Some(s) = pat.score(hay, &mut matcher) {
            scored.push((s, i));
        }
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, i)| i).collect()
}

/// Matched char indices within the DISPLAY text (for highlight).
fn match_indices(entry: &str, pat: &Pattern) -> Vec<u32> {
    let (text, off) = display_span(entry);
    let mut matcher = Matcher::new(Config::DEFAULT);
    let mut buf = Vec::new();
    let hay = Utf32Str::new(entry, &mut buf);
    let mut idx: Vec<u32> = vec![];
    if pat.indices(hay, &mut matcher, &mut idx).is_none() {
        return vec![];
    }
    idx.sort_unstable();
    idx.dedup();
    let n = text.chars().count() as u32;
    idx.into_iter()
        .filter_map(|i| i.checked_sub(off as u32))
        .filter(|i| *i < n)
        .collect()
}

/// Run the picker. Returns (query, selection?) or None on abort.
pub fn run(entries: &[String]) -> Option<(String, Option<String>)> {
    let mut term =
        Terminal::new(CrosstermBackend::new(std::io::BufWriter::new(io::stderr()))).ok()?;
    crossterm::terminal::enable_raw_mode().ok()?;
    let _alt = AltGuard::default();
    {
        use crossterm::ExecutableCommand;
        let _ = io::stderr().execute(crossterm::event::EnableMouseCapture);
        // Focus reporting (DECSET 1004): harmless if the terminal ignores it.
        let _ = io::stderr().execute(crossterm::event::EnableFocusChange);
    }
    let mut query = String::new();
    let mut cursor: usize = 0;
    let mut offset: usize = 0;
    let mut drag_grab: Option<usize> = None;
    // Double-click opens (fzf parity: double-click = Enter).
    let mut last_click: Option<(usize, Instant)> = None;
    let mut pending: Option<Event> = None;
    let mut err_streak: usize = 0;
    let mut focused = true;
    // Event ticker (diagnostic + life sign): last event glyph + count.
    let mut ev_last: char = '·';
    let mut ev_count: u64 = 0;
    // Inline answer state (? / ?? questions).
    let mut ans_lines: Vec<String> = vec![];
    let mut ans_scroll: usize = 0;
    let mut ans_gen: u64 = 0;
    let mut ans_active: Option<u64> = None;
    let mut ans_child: Option<Child> = None;
    let mut ans_rx: Option<Receiver<(u64, AnsEvt)>> = None;
    let mut ans_agent: &'static str = "opencode";
    let mut ans_last_q = String::new();
    let mut ans_session: Option<String> = None;
    let mut ans_shell = false;
    let mut ans_exit: Option<i32> = None;
    let mut last_type = Instant::now();
    // Beauty state: tail-follow, spinner frame, frame-pacing key.
    let mut ans_pinned = true;
    let mut spin: usize = 0;
    let mut last_key: Option<(String, usize, usize, usize, usize, bool, usize, usize, usize, bool, u64)> = None;
    // Single source of geometry truth: the LAST DRAWN layout.
    // (list_y, list_h, scroll_x). Events map to pixels that exist.
    let mut journal = Journal::open();
    journal.line(format!(
        "start version={} entries={} size={:?}",
        env!("CARGO_PKG_VERSION"),
        entries.len(),
        term.size().map(|s| (s.width, s.height)).unwrap_or((0, 0))
    ));
    // (list_y, list_h, scroll_x) tuples follow below in geo.
    let mut geo: (usize, usize, usize) = (4, 20, 98);

    loop {
        let pat = if query.is_empty() {
            None
        } else {
            Some(Pattern::parse(&query, CaseMatching::Smart, Normalization::Smart))
        };
        let order = match &pat {
            Some(p) => rank_with(entries, p),
            None => (0..entries.len()).collect(),
        };
        if cursor >= order.len() && !order.is_empty() {
            cursor = order.len() - 1;
        }
        // Pump streamed events (ignore stale generations).
        // Shell lines are raw text; agent lines are opencode JSONL.
        if let Some(rx) = &ans_rx {
            while let Ok((g, m)) = rx.try_recv() {
                if Some(g) != ans_active {
                    continue;
                }
                match m {
                    AnsEvt::Line(l) => {
                        if ans_shell {
                            // Drop zshrc init noise (`(eval):1: ...`), keep real output.
                            if !l.starts_with("(eval)") {
                                ans_lines.push(l);
                            }
                        } else {
                            let p = parse_json_line(&l);
                            if let Some(s) = p.session {
                                ans_session = Some(s);
                            }
                            if let Some(e) = p.error {
                                ans_lines.push(format!("⚠ {}", e.lines().next().unwrap_or("error")));
                            }
                            for t in p.texts {
                                // Skip exact-duplicate consecutive blocks (re-emitted parts).
                                let dup = ans_lines.last().is_some_and(|last| last == &t);
                                if !dup {
                                    ans_lines.extend(t.lines().map(|s| s.to_string()));
                                }
                            }
                        }
                        // Cap: keep the tail (2000), adjust scroll.
                        if ans_lines.len() > 2000 {
                            let drop = ans_lines.len() - 2000;
                            ans_lines.drain(..drop);
                            ans_scroll = ans_scroll.saturating_sub(drop);
                        }
                    }
                    AnsEvt::Done => {
                        // One EOF is not death (two streams): finalize only
                        // when the child really exited, else keep waiting.
                        let dead = ans_child
                            .as_mut()
                            .map(|c| c.try_wait().ok().flatten().is_some())
                            .unwrap_or(true);
                        if dead {
                            if let Some(c) = ans_child.as_mut() {
                                if let Ok(Some(st)) = c.try_wait() {
                                    ans_exit = st.code();
                                }
                            }
                            ans_active = None;
                            kill_answer(&mut ans_child);
                        }
                    }
                }
            }
        }
        // Launch agent/shell after 600ms idle. ans_last_q prevents
        // relaunching after Done for the same text. New turns APPEND
        // a ❯ block to the transcript (conversation, never cleared).
        if let Some(prompt) = is_shell(&query) {
            ans_shell = true;
            if ans_active.is_none() && query != ans_last_q && last_type.elapsed() > Duration::from_millis(600) {
                ans_gen += 1;
                let (tx, rx) = mpsc::channel();
                ans_rx = Some(rx);
                ans_active = Some(ans_gen);
                ans_last_q = query.clone();
                ans_exit = None;
                ans_pinned = true;
                ans_lines.push(format!("{}{}", QBLOCK, prompt));
                ans_child = spawn_shell(&prompt, ans_gen, tx);
                if ans_child.is_none() {
                    ans_active = None;
                    ans_lines.push("(no se pudo lanzar el shell)".into());
                }
            }
        } else if let Some((agent, prompt)) = is_question(&query) {
            ans_agent = agent;
            ans_shell = false;
            if ans_active.is_none() && query != ans_last_q && last_type.elapsed() > Duration::from_millis(600) {
                ans_gen += 1;
                let (tx, rx) = mpsc::channel();
                ans_rx = Some(rx);
                ans_active = Some(ans_gen);
                ans_last_q = query.clone();
                ans_exit = None;
                ans_pinned = true;
                ans_lines.push(format!("{}{}", QBLOCK, prompt));
                ans_child = spawn_answer(agent, &prompt, ans_gen, ans_session.clone(), tx);
                if ans_child.is_none() {
                    ans_active = None;
                    ans_lines.push("(no se pudo lanzar el agente)".into());
                }
            }
        }
        // Answer view shows while a run is live, or while the query is a
        // question/shell with transcript history. Plain queries show entries.
        let in_ask = is_shell(&query).is_some() || is_question(&query).is_some();
        let answering = ans_active.is_some() || (in_ask && !ans_lines.is_empty());
        // Geometry comes from the last drawn frame (never recomputed from
        // term.size here): clicks and follow map to pixels that exist.
        let list_y = geo.0;
        let view_h = geo.1.max(1);
        let scroll_x = geo.2;
        let max_off = order.len().saturating_sub(view_h);
        if offset > max_off {
            offset = max_off;
        }
        // fzf parity: the selection never leaves the screen.
        // The wheel moves the CURSOR (with acceleration); the viewport
        // follows it here. Keyboard does the same in its own arms.
        if cursor < offset {
            offset = cursor;
        } else if cursor >= offset + view_h {
            offset = cursor + 1 - view_h;
        }
        journal.state(cursor, offset, order.len(), view_h, query.len(), answering, ev_count);
        // Answer scroll: clamp every frame (resize-safe) + tail-follow.
        let ans_max = ans_lines.len().saturating_sub(view_h);
        if ans_scroll > ans_max {
            ans_scroll = ans_max;
        }
        if answering && ans_pinned {
            ans_scroll = ans_max;
        }
        if ans_active.is_some() {
            spin += 1;
        }
        // Frame pacing: skip identical frames (rhythm without storms).
        // term size only joins the key (resize detection), never the logic.
        let (kw, kh) = term
            .size()
            .map(|s| (s.width as usize, s.height as usize))
            .unwrap_or((100, 25));
        let key = (
            query.clone(),
            cursor,
            offset,
            ans_scroll,
            ans_lines.len(),
            ans_active.is_some(),
            spin,
            kw,
            kh,
            focused,
            ev_count,
        );
        if last_key.as_ref() != Some(&key) {
            last_key = Some(key);
            let mut drawn_geo = (list_y, view_h, scroll_x);
            let _ = term.draw(|f| {
            let area = f.area();
            let chrome = if focused { BORDER } else { Color::DarkGray };
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(chrome))
                .border_type(ratatui::widgets::BorderType::Rounded);
            let inner = block.inner(area);
            f.render_widget(block, area);
            // Header row only when it carries live state (answering,
            // unfocused). Otherwise the list takes that row.
            let show_header = answering || !focused;
            let rows = Layout::default()
                .direction(Direction::Vertical)
                .constraints(if show_header {
                    vec![
                        Constraint::Length(1),
                        Constraint::Length(1),
                        Constraint::Length(1),
                        Constraint::Min(1),
                    ]
                } else {
                    vec![
                        Constraint::Length(1),
                        Constraint::Length(1),
                        Constraint::Min(1),
                    ]
                })
                .split(inner);
            let (header_row, list_row) = if show_header {
                (Some(rows[2]), rows[3])
            } else {
                (None, rows[2])
            };
            let prompt_line = format!("   ⚡ {}", query);
            let ans_flag = if answering { format!(" A{}", ans_lines.len()) } else { String::new() };
            let status_line = format!("  {}/{} #{} o{} {}{}{} e{}", order.len(), entries.len(), cursor, offset, ev_last, ev_count, ans_flag, err_streak);
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(prompt_line.as_str(), Style::default().fg(PROMPT).add_modifier(Modifier::BOLD)),
                    Span::styled(status_line, Style::default().fg(BORDER)),
                ])),
                rows[0],
            );
            f.render_widget(
                Paragraph::new("─".repeat(rows[1].width as usize))
                    .style(Style::default().fg(BORDER)),
                rows[1],
            );
            let header_txt = if !focused && !answering {
                "   ◌ sin foco — clic en el popup para enfocar".into()
            } else if answering {
                if ans_shell {
                    if ans_active.is_some() {
                        format!("   {} ⚙ corriendo… (Enter copia y sale, Esc mata)", SPIN[spin % SPIN.len()])
                    } else {
                        match ans_exit {
                            Some(0) => "   ✔ exit 0 — Enter copia y sale, Esc cierra".into(),
                            Some(c) => format!("   ✘ exit {} — Enter copia y sale, Esc cierra", c),
                            None => "   ⚙ terminado — Enter copia y sale, Esc cierra".into(),
                        }
                    }
                } else if ans_active.is_some() {
                    format!("   {} {} está respondiendo… (Enter copia y sale, Esc cierra)", SPIN[spin % SPIN.len()], ans_agent)
                } else {
                    format!("   ✦ respuesta de {} (Enter copia y sale, Esc cierra)", ans_agent)
                }
            } else {
                "   Escribe para buscar o ejecutar (Enter = correr lo escrito)".into()
            };
            if let Some(hr) = header_row {
                f.render_widget(
                    Paragraph::new(header_txt)
                        .style(Style::default().fg(HEADER)),
                    hr,
                );
            }
            // Content width: indent + 1 column reserved for scrollbar.
            // drawn_h is the REAL list height (layout truth, not term.size).
            let drawn_h = (list_row.height as usize).max(1);
            drawn_geo = (
                list_row.y as usize,
                drawn_h,
                list_row.right().saturating_sub(1) as usize,
            );
            let content_w = (list_row.width as usize).saturating_sub(1);
            let items: Vec<ListItem> = if answering {
                ans_lines
                    .iter()
                    .skip(ans_scroll)
                    .take(drawn_h)
                    .map(|l| {
                        let fitted = fit(l, content_w.saturating_sub(2));
                        if fitted.starts_with(QBLOCK) {
                            // User turn: accent voice, stands out in history.
                            ListItem::new(Line::from(vec![Span::styled(
                                format!("  {}", fitted),
                                Style::default().fg(MARK).add_modifier(Modifier::BOLD),
                            )]))
                        } else {
                            ListItem::new(Line::from(md_spans(&fitted, Style::default().fg(FG))))
                        }
                    })
                    .collect()
            } else { order
                .iter()
                .enumerate()
                .skip(offset)
                .take(drawn_h)                .map(|(vi, &ei)| {
                    // vi already counts from 0 of `order` (skip doesn't
                    // renumber), so it compares straight against cursor.
                    let selected = vi == cursor;
                    let mark_style = if selected {
                        Style::default().fg(MARK).bg(Color::Black).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::Black)
                    };
                    let text_style = if selected {
                        Style::default().fg(Color::White).bg(Color::Black).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(FG)
                    };
                    let (text, _) = display_span(&entries[ei]);
                    let fitted = fit(&text, content_w.saturating_sub(4));
                    if selected || pat.is_none() {
                        ListItem::new(Line::from(vec![
                            Span::raw(" "),
                            Span::styled("▌", mark_style),
                            Span::styled(format!("  {}", fitted), text_style),
                        ]))
                    } else {
                        let hits = match_indices(&entries[ei], pat.as_ref().unwrap());
                        let n = fitted.chars().count() as u32;
                        let hits: Vec<u32> = hits.into_iter().filter(|i| *i < n).collect();
                        let mut spans = vec![Span::raw(" "), Span::styled("▌", mark_style), Span::raw("  ")];
                        spans.extend(hl_spans(
                            &fitted,
                            &hits,
                            text_style,
                            Style::default().fg(HL).add_modifier(Modifier::BOLD),
                        ));
                        ListItem::new(Line::from(spans))
                    }
                })
                .collect()
            };
            f.render_widget(List::new(items), list_row);
            // Scrollbar (fzf style): dim track + bright thumb on last column.
            // In answer mode it follows ans_scroll over the answer lines.
            let (sb_total, sb_off) = if answering { (ans_lines.len(), ans_scroll) } else { (order.len(), offset) };
            if sb_total > drawn_h {
                let list_area = list_row;
                let sx = list_area.right().saturating_sub(1);
                let (start, th) = thumb(drawn_h, sb_total, sb_off);
                let buf = f.buffer_mut();
                for i in 0..drawn_h {
                    let y = list_area.y + i as u16;
                    if y >= list_area.bottom() {
                        break;
                    }
                    let is_thumb = i >= start && i < start + th;
                    buf[(sx, y)]
                        .set_symbol("│")
                        .set_fg(if is_thumb { Color::White } else { BORDER });
                }
            }
            f.set_cursor_position((rows[0].x + cell_w(&format!("   ⚡ {}", query)), rows[0].y));
            });
            geo = drawn_geo;
        }
        // draw() flushes the buffer: single write per frame (no tearing).
        // Cursor stays visible the whole time, parked at input (fzf style).

        // Poll (not block): lets streamed answer lines paint while idle.
        // Error budget: a poisoned fd (unparsable bytes stuck in the reader)
        // would deafen us forever; exit clean instead of freezing.
        let first = match pending.take() {
            Some(e) => {
                err_streak = 0;
                Some(e)
            }
            None => {
                if event::poll(Duration::from_millis(150)).unwrap_or(false) {
                    match event::read() {
                        Ok(e) => {
                            err_streak = 0;
                            Some(e)
                        }
                        Err(_) => {
                            err_streak += 1;
                            if err_streak > 200 {
                                kill_answer(&mut ans_child);
                                return None;
                            }
                            None
                        }
                    }
                } else {
                    err_streak = 0;
                    None
                }
            }
        };
        let Some(first) = first else { continue };
        // Coalesce bursts, redraw once per burst:
        // - drag: keep latest position (position-based, intermediates useless).
        // - wheel: accumulate same-direction ticks (trackpads fire dozens/sec;
        //   movement follows the finger: slow = 1 row, flick = up to cap).
        let mut ev = first;
        let mut wheel_acc: usize = 1;
        loop {
            let wheel_dir = match &ev {
                Event::Mouse(m) => match m.kind {
                    MouseEventKind::ScrollUp => Some(true),
                    MouseEventKind::ScrollDown => Some(false),
                    _ => None,
                },
                _ => None,
            };
            let is_drag = matches!(ev, Event::Mouse(m) if matches!(m.kind, MouseEventKind::Drag(_)));
            if wheel_dir.is_none() && !is_drag {
                break;
            }
            if !event::poll(std::time::Duration::ZERO).unwrap_or(false) {
                break;
            }
            match event::read() {
                Ok(Event::Mouse(nm)) if matches!(nm.kind, MouseEventKind::Drag(_)) && is_drag => {
                    ev = Event::Mouse(nm);
                }
                Ok(Event::Mouse(nm)) => {
                    let same_dir = match nm.kind {
                        MouseEventKind::ScrollUp => wheel_dir == Some(true),
                        MouseEventKind::ScrollDown => wheel_dir == Some(false),
                        _ => false,
                    };
                    if same_dir {
                        wheel_acc += 1;
                        ev = Event::Mouse(nm);
                        // Cap per frame BUT stop draining: leftovers stay queued
                        // for next frames (glide), never dropped.
                        if wheel_acc >= 12 {
                            break;
                        }
                    } else {
                        pending = Some(Event::Mouse(nm));
                        break;
                    }
                }
                Ok(other) => {
                    pending = Some(other);
                    break;
                }
                Err(_) => break,
            }
        }
        match ev {
            Event::FocusGained => {
                focused = true;
                ev_count += 1;
                ev_last = 'F';
            }
            Event::FocusLost => {
                focused = false;
                ev_count += 1;
                ev_last = 'f';
            }
            Event::Key(k) => {
                ev_count += 1;
                ev_last = match k.code {
                    KeyCode::Up => '↑',
                    KeyCode::Down => '↓',
                    KeyCode::Enter => '⏎',
                    KeyCode::Esc => '⎋',
                    KeyCode::Backspace => '⌫',
                    KeyCode::Char(c) => c,
                    _ => '·',
                };
                match (k.code, k.modifiers) {
            (KeyCode::Esc, _) => {
                kill_answer(&mut ans_child);
                journal.line("exit esc".into());
                return None;
            }
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                kill_answer(&mut ans_child);
                journal.line("exit ctrlc".into());
                return None;
            }
            (KeyCode::Enter, _) => {
                kill_answer(&mut ans_child);
                if in_ask || answering {
                    // Conversación: copiar la ÚLTIMA respuesta y salir.
                    // Sin respuesta aún (Enter rápido): lanzar ya y quedarse.
                    let la = last_answer(&ans_lines);
                    if !la.is_empty() {
                        copy_answer(&la);
                        journal.line("exit enter-copy".into());
                        return Some((String::new(), None));
                    }
                    ans_active = None;
                    ans_last_q = String::new();
                    ans_shell = false;
                    ans_exit = None;
                    last_type = Instant::now() - Duration::from_secs(3600);
                } else {
                    let sel = order.get(cursor).map(|&i| entries[i].clone());
                    journal.line(format!("exit enter sel={}", sel.is_some() as u8));
                    return Some((query, sel));
                }
            }
            (KeyCode::Backspace, _) => {
                query.pop();
                cursor = 0;
                drag_grab = None;
                reset_answer(&mut ans_child, &mut ans_active);
                ans_shell = false;
                ans_exit = None;
                last_click = None;
                last_type = Instant::now();
            }
            (KeyCode::Up, _) | (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
                if answering {
                    ans_scroll = ans_scroll.saturating_sub(1);
                    ans_pinned = false;
                } else {
                    cursor = cursor.saturating_sub(1);
                }
            }
            (KeyCode::Down, _) | (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
                if answering {
                    let max_s = ans_lines.len().saturating_sub(view_h);
                    ans_scroll = (ans_scroll + 1).min(max_s);
                    ans_pinned = ans_scroll == max_s;
                } else if cursor + 1 < order.len() {
                    cursor += 1;
                }
            }
            (KeyCode::Char(ch), KeyModifiers::NONE) | (KeyCode::Char(ch), KeyModifiers::SHIFT) => {
                query.push(ch);
                cursor = 0;
                drag_grab = None;
                reset_answer(&mut ans_child, &mut ans_active);
                ans_shell = false;
                ans_exit = None;
                last_click = None;
                last_type = Instant::now();
            }
            _ => {}
            }
            },
            Event::Mouse(m) => {
                ev_count += 1;
                ev_last = match m.kind {
                    MouseEventKind::ScrollUp => '▲',
                    MouseEventKind::ScrollDown => '▼',
                    MouseEventKind::Down(_) => '●',
                    MouseEventKind::Drag(_) => '◎',
                    MouseEventKind::Up(_) => '○',
                    _ => '·',
                };
                let max_cur = order.len().saturating_sub(1);
                // Answer mode scrolls ans_lines with the same scrollbar math.
                let (sb_total, sb_off) = if answering { (ans_lines.len(), ans_scroll) } else { (order.len(), offset) };
                let set_ans_scroll = |v: usize, view_h: usize, len: usize| v.min(len.saturating_sub(view_h));
                match m.kind {
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                        // wheel_acc comes from burst coalescing above:
                        // slow finger = 1 row, fast flick = up to 12.
                        let step = wheel_acc;
                        if answering {
                            if m.kind == MouseEventKind::ScrollUp {
                                ans_scroll = ans_scroll.saturating_sub(step);
                            } else {
                                ans_scroll = set_ans_scroll(ans_scroll + step, view_h, ans_lines.len());
                            }
                            ans_pinned = ans_scroll == ans_lines.len().saturating_sub(view_h);
                        } else if m.kind == MouseEventKind::ScrollUp {
                            cursor = cursor.saturating_sub(step);
                        } else {
                            cursor = (cursor + step).min(max_cur);
                        }
                    }
                    MouseEventKind::Down(MouseButton::Left) => {
                        let row = m.row as usize;
                        if sb_total > view_h
                            && m.column as usize == scroll_x
                            && row >= list_y
                            && row < list_y + view_h
                        {
                            let r = row - list_y;
                            let (th_start, th_len) = thumb(view_h, sb_total, sb_off);
                            let grab = if r >= th_start && r < th_start + th_len {
                                // Grabbed the thumb: no jump, follow the finger.
                                r - th_start
                            } else {
                                // Clicked the track: jump, grab middle for smooth drag.
                                th_len / 2
                            };
                            let new_off = row_to_offset(r, grab, view_h, th_len, sb_total);
                            if answering {
                                ans_scroll = new_off;
                                ans_pinned = ans_scroll == ans_lines.len().saturating_sub(view_h);
                            } else {
                                offset = new_off;
                                cursor = cursor.clamp(offset, (offset + view_h - 1).min(max_cur));
                            }
                            drag_grab = Some(grab);
                        } else if row >= list_y && row < list_y + view_h && !order.is_empty() {
                            if answering {
                                let r = row - list_y;
                                ans_scroll = set_ans_scroll(r * sb_total / view_h, view_h, sb_total);
                                ans_pinned = ans_scroll == ans_lines.len().saturating_sub(view_h);
                            } else {
                                let target = (offset + row - list_y).min(max_cur);
                                // Double-click opens (fzf parity): same row twice fast.
                                let now = Instant::now();
                                if last_click.is_some_and(|(t, at)| t == target && now - at < Duration::from_millis(500)) {
                                    kill_answer(&mut ans_child);
                                    let sel = order.get(target).map(|&i| entries[i].clone());
                                    journal.line(format!("exit dblclick sel={}", sel.is_some() as u8));
                                    return Some((query, sel));
                                }
                                cursor = target;
                                last_click = Some((target, now));
                            }
                            drag_grab = None;
                        }
                    }
                    MouseEventKind::Drag(MouseButton::Left) => {
                        if sb_total <= view_h || order.is_empty() && !answering {
                        } else if let Some(grab) = drag_grab {
                            // Scrollbar drag: inverse mapping, clamped to ends.
                            let r = (m.row as usize).clamp(list_y, list_y + view_h - 1) - list_y;
                            let (_, th_len) = thumb(view_h, sb_total, sb_off);
                            let new_off = row_to_offset(r, grab.min(view_h), view_h, th_len, sb_total);
                            if answering {
                                ans_scroll = new_off;
                                ans_pinned = ans_scroll == ans_lines.len().saturating_sub(view_h);
                            } else {
                                offset = new_off;
                                cursor = cursor.clamp(offset, (offset + view_h - 1).min(max_cur));
                            }
                        } else if (m.row as usize) >= list_y && (m.row as usize) < list_y + view_h {
                            if answering {
                                let r = (m.row as usize) - list_y;
                                ans_scroll = set_ans_scroll(r * sb_total / view_h, view_h, sb_total);
                                ans_pinned = ans_scroll == ans_lines.len().saturating_sub(view_h);
                            } else {
                                cursor = (offset + (m.row as usize) - list_y).min(max_cur);
                            }
                        }
                    }
                    MouseEventKind::Up(_) => {
                        drag_grab = None;
                    }
                    _ => {
                        ev_count += 1;
                        ev_last = '?';
                    }
                }
            }
            Event::Resize(_, _) => {
                ev_count += 1;
                ev_last = 'R';
            }
            Event::Paste(_) => {
                ev_count += 1;
                ev_last = 'P';
            }
        }
    }
}

struct AltGuard;
impl AltGuard {
    fn enter() {
        use crossterm::ExecutableCommand;
        let _ = io::stderr().execute(crossterm::terminal::EnterAlternateScreen);
    }
}
impl Default for AltGuard {
    fn default() -> Self {
        Self::enter();
        AltGuard
    }
}
impl Drop for AltGuard {
    fn drop(&mut self) {
        use crossterm::ExecutableCommand;
        let _ = io::stderr().execute(crossterm::event::DisableMouseCapture);
        let _ = io::stderr().execute(crossterm::event::DisableFocusChange);
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = io::stderr().execute(crossterm::terminal::LeaveAlternateScreen);
        let _ = io::stderr().execute(crossterm::cursor::Show);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<String> {
        vec![
            "/opt/homebrew/bin/yazi | yazi → file manager".to_string(),
            "/opt/homebrew/bin/music-cli | msc → music".to_string(),
            "/opt/homebrew/bin/lazygit | lg → git ui".to_string(),
        ]
    }

    #[test]
    fn empty_query_keeps_frecency_order() {
        let e = sample();
        assert_eq!(rank_pub(&e, ""), vec![0, 1, 2]);
    }

    #[test]
    fn abbreviations_hit_expected_entry_first() {
        let e = sample();
        assert_eq!(rank_pub(&e, "yazi")[0], 0);
        assert_eq!(rank_pub(&e, "msc")[0], 1);
        assert_eq!(rank_pub(&e, "lg")[0], 2);
    }

    #[test]
    fn no_match_gives_empty() {
        let e = sample();
        assert!(rank_pub(&e, "zzzqqq").is_empty());
    }

    #[test]
    fn display_shows_command_part() {
        assert_eq!(display_span("/x | msc → music").0, "msc → music");
        assert_eq!(display_span("/x | msc → music").1, 5);
        assert_eq!(display_span("plain"), ("plain".into(), 0));
    }

    #[test]
    fn fit_truncates_by_width() {
        assert_eq!(fit("hola", 10), "hola");
        assert_eq!(fit("hola mundo", 7), "hola m…");
        assert_eq!(fit("abc", 3), "abc");
    }

    #[test]
    fn match_indices_hit_display() {
        let pat = Pattern::parse("msc", CaseMatching::Smart, Normalization::Smart);
        let hits = match_indices("/bin | msc → music", &pat);
        // display "msc → music": m,s,c at 0,1,2.
        assert!(hits.contains(&0) && hits.contains(&1) && hits.contains(&2));
        let pat2 = Pattern::parse("zzz", CaseMatching::Smart, Normalization::Smart);
        assert!(match_indices("/bin | msc → music", &pat2).is_empty());
    }

    #[test]
    fn question_detect() {
        assert_eq!(is_question("?hola").unwrap().0, "opencode");
        assert_eq!(is_question("?hola").unwrap().1, "hola");
        assert_eq!(is_question("??haz x").unwrap().0, "opencode");
        assert!(is_question("?").is_none());
        assert!(is_question("??").is_none());
        assert!(is_question("?  ").is_none());
        assert!(is_question("yazi").is_none());
        assert!(is_question("msc --help").is_none());
    }

    #[test]
    fn shell_detect() {
        assert_eq!(is_shell(">brew uninstall foo").as_deref(), Some("brew uninstall foo"));
        assert_eq!(is_shell(">  ls -la  ").as_deref(), Some("ls -la"));
        assert!(is_shell(">").is_none());
        assert!(is_shell(">  ").is_none());
        assert!(is_shell("yazi").is_none());
        assert!(is_shell("a > b").is_none());
    }

    #[test]
    fn last_answer_takes_final_turn() {
        let lines = vec![
            "❯ primera".to_string(),
            "resp 1".to_string(),
            "❯ segunda".to_string(),
            "resp 2a".to_string(),
            "resp 2b".to_string(),
        ];
        assert_eq!(last_answer(&lines), vec!["resp 2a", "resp 2b"]);
        assert!(last_answer(&["❯ pensando".to_string()]).is_empty());
        assert!(last_answer(&[]).is_empty());
    }

    #[test]
    fn json_text_shapes() {
        let p = parse_json_line(r#"{"type":"text","text":"hola","sessionID":"ses_1","timestamp":1}"#);
        assert_eq!(p.texts, vec!["hola"]);
        assert_eq!(p.session.as_deref(), Some("ses_1"));
        let p = parse_json_line(r#"{"type":"text","part":{"text":"mundo"},"sessionID":"ses_2"}"#);
        assert_eq!(p.texts, vec!["mundo"]);
        let p = parse_json_line(r#"{"type":"text","properties":{"part":{"text":"x"}}}"#);
        assert_eq!(p.texts, vec!["x"]);
        // Noise types yield nothing.
        let p = parse_json_line(r#"{"type":"tool_use","sessionID":"ses_1"}"#);
        assert!(p.texts.is_empty() && p.error.is_none());
        assert_eq!(p.session.as_deref(), Some("ses_1"));
        let p = parse_json_line(r#"{"type":"reasoning","text":"pensando"}"#);
        assert!(p.texts.is_empty());
        let p = parse_json_line(r#"{"type":"step_start","sessionID":"ses_9"}"#);
        assert_eq!(p.session.as_deref(), Some("ses_9"));
        let p = parse_json_line(r#"{"type":"error","error":{"message":"boom"}}"#);
        assert_eq!(p.error.as_deref(), Some("boom"));
        // Garbage never panics, never shows noise.
        let p = parse_json_line("esto no es json");
        assert!(p.texts.is_empty() && p.session.is_none() && p.error.is_none());
    }

    #[test]
    fn thumb_top_and_bottom() {
        assert_eq!(thumb(22, 1688, 0), (0, 1));
        let (s, l) = thumb(22, 1688, 1688 - 22);
        assert_eq!((s + l), 22);
    }

    #[test]
    fn thumb_no_scroll_when_fits() {
        assert_eq!(thumb(30, 10, 0), (0, 30));
        assert_eq!(row_to_offset(5, 0, 30, 30, 10), 0);
    }

    #[test]
    fn row_mapping_roundtrips() {
        // row->offset->thumb-start must land back within one thumb-step.
        let (vh, total) = (22, 1688);
        let step = (total - vh) / vh + 1;
        for off in [0, 100, 800, 1666] {
            let (ts, tl) = thumb(vh, total, off);
            let back = row_to_offset(ts, 0, vh, tl, total);
            assert!((back as isize - off as isize).abs() <= step as isize, "off={off} back={back}");
        }
    }
}
