//! Native fuzzy picker (nucleo + Ratatui): fzf parity without subprocess.
//! Same look (prompt ⚡, rounded border, nord-ish colors, reverse layout),
//! same semantics: Enter runs selection (or typed query), Esc aborts.

use crossterm::event::{self, Event, KeyCode, KeyModifiers, MouseButton, MouseEventKind};
use nucleo::{
    pattern::{CaseMatching, Normalization, Pattern},
    Config, Matcher, Utf32Str,
};
use std::time::Instant;
use ratatui::{
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph},
    Terminal,
};
use ratatui::backend::CrosstermBackend;
use std::io;

const FG: Color = Color::Rgb(0x81, 0xa1, 0xc1);
const PROMPT: Color = Color::Rgb(0x8f, 0xbc, 0xbb);
const BORDER: Color = Color::Rgb(0x5e, 0x81, 0xac);
const HEADER: Color = Color::Indexed(109);
const MARK: Color = Color::Indexed(161);

fn cell_w(s: &str) -> u16 {
    s.chars().map(|c| if c.is_ascii() { 1 } else { 2 }).sum()
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

/// Display part of an entry (fzf --with-nth=2 equivalent).
pub fn display(entry: &str) -> String {
    entry.split('|').nth(1).map(|s| s.trim().to_string()).unwrap_or_else(|| entry.to_string())
}

fn rank(entries: &[String], query: &str) -> Vec<usize> {
    if query.is_empty() {
        return (0..entries.len()).collect();
    }
    let pat = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
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

/// Run the picker. Returns (query, selection?) or None on abort.
pub fn run(entries: &[String]) -> Option<(String, Option<String>)> {
    let mut term =
        Terminal::new(CrosstermBackend::new(std::io::BufWriter::new(io::stderr()))).ok()?;
    crossterm::terminal::enable_raw_mode().ok()?;
    let _alt = AltGuard::default();
    {
        use crossterm::ExecutableCommand;
        let _ = io::stderr().execute(crossterm::event::EnableMouseCapture);
    }
    let mut query = String::new();
    let mut cursor: usize = 0;
    let mut offset: usize = 0;
    let mut last_wheel: Option<Instant> = None;
    let mut wheel_step: usize = 1;
    let mut drag_grab: Option<usize> = None;
    let mut pending: Option<Event> = None;

    loop {
        let order = rank(entries, &query);
        if cursor >= order.len() && !order.is_empty() {
            cursor = order.len() - 1;
        }
        // Visible window follows the cursor (fzf viewport behavior).
        // Layout (fullscreen alt-screen, origin 0,0): border 0, inner x=1,
        // prompt y=1, separator y=2, header y=3, list y=4.., scrollbar x=w-2.
        let (term_w, view_h) = term
            .size()
            .map(|s| (s.width as usize, (s.height as usize).saturating_sub(5).max(1)))
            .unwrap_or((100, 20));
        let list_y: usize = 4;
        let scroll_x: usize = term_w.saturating_sub(2);
        let max_off = order.len().saturating_sub(view_h);
        if offset > max_off {
            offset = max_off;
        }
        if cursor < offset {
            offset = cursor;
        } else if cursor >= offset + view_h {
            offset = cursor + 1 - view_h;
        }
        let _ = term.draw(|f| {
            let area = f.area();
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(BORDER))
                .border_type(ratatui::widgets::BorderType::Rounded);
            let inner = block.inner(area);
            f.render_widget(block, area);
            let rows = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(1),
                    Constraint::Length(1),
                    Constraint::Length(1),
                    Constraint::Min(1),
                ])
                .split(inner);
            let prompt_line = format!("   ⚡ {}", query);
            f.render_widget(
                Paragraph::new(prompt_line.as_str()).style(Style::default().fg(PROMPT).add_modifier(Modifier::BOLD)),
                rows[0],
            );
            f.render_widget(
                Paragraph::new("─".repeat(rows[1].width as usize))
                    .style(Style::default().fg(BORDER)),
                rows[1],
            );
            f.render_widget(
                Paragraph::new("   Escribe para buscar o ejecutar (Enter = correr lo escrito)")
                    .style(Style::default().fg(HEADER)),
                rows[2],
            );
            let items: Vec<ListItem> = order
                .iter()
                .enumerate()
                .skip(offset)
                .take(view_h)                .map(|(vi, &ei)| {
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
                    ListItem::new(Line::from(vec![
                        Span::raw(" "),
                        Span::styled("▌", mark_style),
                        Span::styled(format!("  {}", display(&entries[ei])), text_style),
                    ]))
                })
                .collect();
            f.render_widget(List::new(items), rows[3]);
            // Scrollbar (fzf style): dim track + bright thumb on last column.
            let total = order.len();
            if total > view_h {
                let list_area = rows[3];
                let sx = list_area.right().saturating_sub(1);
                let (start, th) = thumb(view_h, total, offset);
                let buf = f.buffer_mut();
                for i in 0..view_h {
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
            f.set_cursor_position((rows[0].x + cell_w(&prompt_line), rows[0].y));
        });
        // draw() flushes the buffer: single write per frame (no tearing).
        // Cursor stays visible the whole time, parked at input (fzf style).

        let Ok(first) = pending.take().map(Ok).unwrap_or_else(event::read) else { break };
        // Coalesce drag bursts: redraw once per burst, not per event.
        let mut ev = first;
        if matches!(ev, Event::Mouse(m) if matches!(m.kind, MouseEventKind::Drag(_))) {
            while event::poll(std::time::Duration::ZERO).unwrap_or(false) {
                match event::read() {
                    Ok(Event::Mouse(nm)) if matches!(nm.kind, MouseEventKind::Drag(_)) => {
                        ev = Event::Mouse(nm);
                    }
                    Ok(other) => {
                        pending = Some(other);
                        break;
                    }
                    Err(_) => break,
                }
            }
        }
        match ev {
            Event::Key(k) => match (k.code, k.modifiers) {
            (KeyCode::Esc, _) => return None,
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => return None,
            (KeyCode::Enter, _) => {
                let sel = order.get(cursor).map(|&i| entries[i].clone());
                return Some((query, sel));
            }
            (KeyCode::Backspace, _) => {
                query.pop();
                cursor = 0;
                drag_grab = None;
            }
            (KeyCode::Up, _) | (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
                cursor = cursor.saturating_sub(1);
            }
            (KeyCode::Down, _) | (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
                if cursor + 1 < order.len() {
                    cursor += 1;
                }
            }
            (KeyCode::Char(ch), KeyModifiers::NONE) | (KeyCode::Char(ch), KeyModifiers::SHIFT) => {
                query.push(ch);
                cursor = 0;
                drag_grab = None;
            }
            _ => {}
            },
            Event::Mouse(m) => {
                let max_cur = order.len().saturating_sub(1);
                let total = order.len();
                match m.kind {
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                        // Natural wheel: 1 by 1, accelerates while spinning fast.
                        let now = Instant::now();
                        if last_wheel.is_some_and(|t| now - t < std::time::Duration::from_millis(150)) {
                            wheel_step = (wheel_step + 1).min(8);
                        } else {
                            wheel_step = 1;
                        }
                        last_wheel = Some(now);
                        if m.kind == MouseEventKind::ScrollUp {
                            cursor = cursor.saturating_sub(wheel_step);
                        } else {
                            cursor = (cursor + wheel_step).min(max_cur);
                        }
                    }
                    MouseEventKind::Down(MouseButton::Left) => {
                        let row = m.row as usize;
                        if total > view_h
                            && m.column as usize == scroll_x
                            && row >= list_y
                            && row < list_y + view_h
                        {
                            let r = row - list_y;
                            let (th_start, th_len) = thumb(view_h, total, offset);
                            if r >= th_start && r < th_start + th_len {
                                // Grabbed the thumb: no jump, follow the finger.
                                drag_grab = Some(r - th_start);
                            } else {
                                // Clicked the track: jump, grab middle for smooth drag.
                                let grab = th_len / 2;
                                offset = row_to_offset(r, grab, view_h, th_len, total);
                                cursor = cursor.clamp(offset, (offset + view_h - 1).min(max_cur));
                                drag_grab = Some(grab);
                            }
                        } else if row >= list_y && row < list_y + view_h && !order.is_empty() {
                            cursor = (offset + row - list_y).min(max_cur);
                            drag_grab = None;
                        }
                    }
                    MouseEventKind::Drag(MouseButton::Left) => {
                        if total <= view_h || order.is_empty() {
                        } else if let Some(grab) = drag_grab {
                            // Scrollbar drag: inverse mapping, clamped to ends.
                            let r = (m.row as usize).clamp(list_y, list_y + view_h - 1) - list_y;
                            let (_, th_len) = thumb(view_h, total, offset);
                            offset = row_to_offset(r, grab.min(view_h), view_h, th_len, total);
                            cursor = cursor.clamp(offset, (offset + view_h - 1).min(max_cur));
                        } else if (m.row as usize) >= list_y && (m.row as usize) < list_y + view_h {
                            cursor = (offset + (m.row as usize) - list_y).min(max_cur);
                        }
                    }
                    MouseEventKind::Up(_) => {
                        drag_grab = None;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    None
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
        assert_eq!(rank(&e, ""), vec![0, 1, 2]);
    }

    #[test]
    fn abbreviations_hit_expected_entry_first() {
        let e = sample();
        assert_eq!(rank(&e, "yazi")[0], 0);
        assert_eq!(rank(&e, "msc")[0], 1);
        assert_eq!(rank(&e, "lg")[0], 2);
    }

    #[test]
    fn no_match_gives_empty() {
        let e = sample();
        assert!(rank(&e, "zzzqqq").is_empty());
    }

    #[test]
    fn display_shows_command_part() {
        assert_eq!(display("/x | msc → music"), "msc → music");
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
