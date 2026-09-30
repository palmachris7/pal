//! Native fuzzy picker (nucleo + Ratatui): fzf parity without subprocess.
//! Same look (prompt ⚡, rounded border, nord-ish colors, reverse layout),
//! same semantics: Enter runs selection (or typed query), Esc aborts.

use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use nucleo::{
    pattern::{CaseMatching, Normalization, Pattern},
    Config, Matcher, Utf32Str,
};
use ratatui::{
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, List, ListItem, Paragraph},
    Terminal,
};
use ratatui::backend::CrosstermBackend;
use std::io;

const FG: Color = Color::Rgb(0x81, 0xa1, 0xc1);
const PROMPT: Color = Color::Rgb(0x8f, 0xbc, 0xbb);
const BORDER: Color = Color::Rgb(0x5e, 0x81, 0xac);

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
    let mut term = Terminal::new(CrosstermBackend::new(io::stderr())).ok()?;
    crossterm::terminal::enable_raw_mode().ok()?;
    let _alt = AltGuard::default();
    let mut query = String::new();
    let mut cursor: usize = 0;

    loop {
        let order = rank(entries, &query);
        if cursor >= order.len() && !order.is_empty() {
            cursor = order.len() - 1;
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
                .constraints([Constraint::Length(1), Constraint::Length(1), Constraint::Min(1)])
                .split(inner);
            f.render_widget(
                Paragraph::new(format!("  ⚡ {}", query)).style(Style::default().fg(PROMPT)),
                rows[0],
            );
            f.render_widget(
                Paragraph::new("Escribe para buscar o ejecutar (Enter = correr lo escrito)")
                    .style(Style::default().fg(FG)),
                rows[1],
            );
            let items: Vec<ListItem> = order
                .iter()
                .enumerate()
                .take(rows[2].height as usize)
                .map(|(vi, &ei)| {
                    let mut st = Style::default().fg(FG);
                    if vi == cursor {
                        st = Style::default().fg(Color::White).bg(Color::Black).add_modifier(Modifier::BOLD);
                    }
                    ListItem::new(display(&entries[ei])).style(st)
                })
                .collect();
            f.render_widget(List::new(items), rows[2]);
        });

        let Ok(ev) = event::read() else { break };
        let Event::Key(k) = ev else { continue };
        match (k.code, k.modifiers) {
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
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = io::stderr().execute(crossterm::terminal::LeaveAlternateScreen);
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
