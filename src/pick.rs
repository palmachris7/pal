//! Native fuzzy picker (nucleo + Ratatui): fzf parity without subprocess.
//! Same look (prompt ⚡, rounded border, nord-ish colors, reverse layout),
//! same semantics: Enter runs selection (or typed query), Esc aborts.

use crossterm::event::{self, Event, KeyCode, KeyModifiers, MouseButton, MouseEventKind};
use nucleo::{
    pattern::{CaseMatching, Normalization, Pattern},
    Config, Matcher, Utf32Str,
};
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
                .take(view_h)
                .map(|(vi, &ei)| {
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
                let th = (view_h * view_h / total).max(1);
                let start = offset * (view_h - th) / (total - view_h).max(1);
                let buf = f.buffer_mut();
                for i in 0..view_h {
                    let y = list_area.y + i as u16;
                    if y >= list_area.bottom() {
                        break;
                    }
                    let thumb = i >= start && i < start + th;
                    buf[(sx, y)]
                        .set_symbol("│")
                        .set_fg(if thumb { Color::White } else { BORDER });
                }
            }
            f.set_cursor_position((rows[0].x + cell_w(&prompt_line), rows[0].y));
        });
        // draw() flushes the buffer: single write per frame (no tearing).
        // Cursor stays visible the whole time, parked at input (fzf style).

        let Ok(ev) = event::read() else { break };
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
            }
            _ => {}
            },
            Event::Mouse(m) => {
                let max_cur = order.len().saturating_sub(1);
                match m.kind {
                    MouseEventKind::ScrollUp => cursor = cursor.saturating_sub(3),
                    MouseEventKind::ScrollDown => {
                        cursor = (cursor + 3).min(max_cur);
                    }
                    MouseEventKind::Down(MouseButton::Left)
                    | MouseEventKind::Drag(MouseButton::Left) => {
                        let row = m.row as usize;
                        if row >= list_y && row < list_y + view_h && !order.is_empty() {
                            if m.column as usize == scroll_x {
                                // Scrollbar jump (proportional).
                                cursor = ((row - list_y) * order.len() / view_h).min(max_cur);
                            } else {
                                cursor = (offset + row - list_y).min(max_cur);
                            }
                        }
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
}
