use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use super::themes::Theme;

/// Render the diff preview pane (FR-5).
pub fn render(
    diff_content: Option<&str>,
    diff_status: &str,
    scroll: u16,
    area: Rect,
    theme: &Theme,
) -> Paragraph<'static> {
    let body = match diff_content {
        Some(content) if !content.is_empty() => {
            let lines: Vec<Line<'static>> = content
                .lines()
                .map(|line| {
                    let color = if line.starts_with('+') && !line.starts_with("+++") {
                        theme.green
                    } else if line.starts_with('-') && !line.starts_with("---") {
                        theme.red
                    } else if line.starts_with("@@") {
                        theme.blue
                    } else if line.starts_with("diff ") || line.starts_with("index ") {
                        theme.muted
                    } else {
                        theme.text
                    };
                    Line::from(Span::styled(line.to_string(), Style::default().fg(color)))
                })
                .collect();
            Text::from(lines)
        }
        _ => Text::from(vec![Line::from(Span::styled(
            diff_status.to_string(),
            Style::default().fg(theme.muted),
        ))]),
    };

    let inner_height = area.height.saturating_sub(2);
    let max_scroll = (body.lines.len() as u16).saturating_sub(inner_height);
    let effective_scroll = scroll.min(max_scroll);

    Paragraph::new(body)
        .block(
            Block::default()
                .title(Span::styled(
                    " diff ",
                    Style::default()
                        .fg(theme.text)
                        .add_modifier(Modifier::BOLD),
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.border).bg(theme.surface))
                .style(Style::default().bg(theme.bg)),
        )
        .style(Style::default().fg(theme.text).bg(theme.bg))
        .wrap(Wrap { trim: false })
        .scroll((effective_scroll, 0))
}
