use crate::{App, clean, editor, string};
use ftui::{
    Frame, Widget,
    core::geometry::Rect,
    layout::{Constraint, Flex},
    style::{Color, Style},
    text::{Line, Span, Text, WrapMode},
    widgets::paragraph::Paragraph,
};
use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

const MAX_MARKDOWN_CACHE_ENTRIES: usize = 256;

struct CachedMarkdown {
    text_len: usize,
    width: usize,
    hash: u64,
    lines: Vec<Line<'static>>,
}

thread_local! {
    static MARKDOWN_CACHE: RefCell<HashMap<usize, CachedMarkdown>> = RefCell::new(HashMap::new());
}

fn hash_str(s: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    s.hash(&mut hasher);
    hasher.finish()
}

fn cached_markdown(idx: usize, text: &str, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let hash = hash_str(text);
    MARKDOWN_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(entry) = cache.get(&idx)
            && entry.text_len == text.len()
            && entry.width == width
            && entry.hash == hash
        {
            return entry.lines.clone();
        }
        let lines: Vec<Line<'static>> = ftui_extras::markdown::render_markdown(text)
            .lines()
            .iter()
            .flat_map(|line| line.wrap(width, WrapMode::WordChar))
            .collect();
        if cache.len() >= MAX_MARKDOWN_CACHE_ENTRIES && !cache.contains_key(&idx) {
            cache.clear();
        }
        cache.insert(
            idx,
            CachedMarkdown {
                text_len: text.len(),
                width,
                hash,
                lines: lines.clone(),
            },
        );
        lines
    })
}

#[cfg(not(target_arch = "wasm32"))]
pub fn clear_markdown_cache() {
    MARKDOWN_CACHE.with(|cache| cache.borrow_mut().clear());
}

fn paint<'a>(frame: &mut Frame, area: Rect, lines: impl IntoIterator<Item = Line<'a>>) {
    Paragraph::new(Text::from_lines(lines)).render(area, frame);
}
#[derive(Clone, Copy)]
#[cfg_attr(target_arch = "wasm32", derive(serde::Deserialize))]
pub enum Role {
    User,
    Assistant,
    Thinking,
    Tool,
    Output,
    Notice,
    Error,
}
#[cfg_attr(target_arch = "wasm32", derive(serde::Deserialize))]
pub struct Message {
    pub role: Role,
    pub text: String,
}

fn transcript<'a>(
    idx: usize,
    msg: &'a Message,
    compact: bool,
    en: bool,
    width: usize,
) -> Vec<Line<'a>> {
    let (marker, style) = match msg.role {
        Role::Assistant => {
            return cached_markdown(idx, &msg.text, width);
        }
        Role::Thinking => ("~ ", Style::new().fg(Color::DarkGray)),
        Role::User => ("› ", Style::new().fg(Color::Cyan)),
        Role::Tool => ("→ ", Style::new().fg(Color::Yellow)),
        Role::Output => ("│ ", Style::new().fg(Color::Gray)),
        Role::Notice => ("· ", Style::new().fg(Color::Gray)),
        Role::Error => ("! ", Style::new().fg(Color::Red)),
    };
    let mut lines = Vec::new();
    let preview = matches!(msg.role, Role::Output) && compact;
    let body_width = width.saturating_sub(2).max(1);
    let word_wrap = matches!(msg.role, Role::User | Role::Notice | Role::Error | Role::Thinking);
    for content in msg.text.split('\n') {
        if word_wrap {
            let wrapped = if content.is_empty() {
                vec![String::new()]
            } else {
                Line::raw(content)
                    .wrap(body_width, WrapMode::WordChar)
                    .into_iter()
                    .map(|l| l.to_plain_text())
                    .collect()
            };
            for row_text in wrapped {
                let prefix = if lines.is_empty() { marker } else { "  " };
                lines.push(Line::from_spans(vec![
                    Span::styled(prefix, style.bold()),
                    Span::styled(row_text, style),
                ]));
            }
            continue;
        }
        for row in editor::rows(content, body_width)
            .into_iter()
            .filter(|r| !r.text.is_empty() || content.is_empty())
        {
            if preview && lines.len() == 3 {
                lines.push(Line::styled(
                    if en {
                        "  … Ctrl+O to expand"
                    } else {
                        "  … Ctrl+O per espandere"
                    },
                    style.dim(),
                ));
                return lines;
            }
            let prefix = if lines.is_empty() || matches!(msg.role, Role::Output) {
                marker
            } else {
                "  "
            };
            lines.push(Line::from_spans(vec![
                Span::styled(prefix, style.bold()),
                Span::styled(row.text, style),
            ]));
        }
    }
    lines
}

pub fn draw(
    f: &mut Frame,
    app: &mut App,
    caret: usize,
    suggestions: &[&str],
    selection: usize,
    spinner: Option<&str>,
    picker_label: Option<&str>,
) {
    let area = f.area();
    let width = usize::from(area.width);
    let rows = editor::rows(&app.input, width.saturating_sub(2));
    let editor_height = rows
        .len()
        .min(5)
        .min(usize::from(area.height.saturating_sub(4)))
        .max(1) as u16;
    let menu_height =
        (suggestions.len().min(5) as u16).min(area.height.saturating_sub(editor_height + 7).max(1));
    let regions = Flex::vertical()
        .constraints([
            Constraint::Fixed(1),
            Constraint::Fill,
            Constraint::Fixed(u16::from(!app.status.is_empty())),
            Constraint::Fixed(1),
            Constraint::Fixed(menu_height),
            Constraint::Fixed(editor_height),
            Constraint::Fixed(1),
        ])
        .split(area);
    let [header, body, banner, status, menu, input, footer] = regions[..] else {
        unreachable!()
    };

    let mut title = vec![Span::styled(
        " Venticode ",
        Style::new().fg(Color::Black).bg(Color::Cyan).bold(),
    )];
    for value in [string(&app.project, "name"), string(&app.chat, "title")] {
        if !value.is_empty() {
            title.push(Span::raw(format!("  {}", clean(&value))));
        }
    }
    paint(f, header, [Line::from_spans(title)]);

    // Traverse from the tail so the redraw cost follows the visible history.
    let needed = app.scroll as usize + body.height as usize;
    let mut history = Vec::new();
    for (idx, msg) in app.lines.iter().enumerate().rev() {
        if !history.is_empty() {
            history.push(Line::default());
        }
        history.extend(
            transcript(idx, msg, app.compact_tools, app.en, width)
                .into_iter()
                .rev(),
        );
        if history.len() >= needed {
            break;
        }
    }
    app.scroll =
        (app.scroll as usize).min(history.len().saturating_sub(body.height as usize)) as u16;
    let skip = app.scroll as usize;
    history.truncate(skip + body.height as usize);
    let visible = history.drain(skip..).rev().collect::<Vec<_>>();
    paint(f, body, visible);
    paint(
        f,
        banner,
        [Line::styled(
            clean(&app.status),
            Style::new().fg(Color::Red).bold(),
        )],
    );
    let activity = if let Some(label) = picker_label {
        label.to_owned()
    } else if !app.approval.is_empty() {
        app.msg("Approvazione · /allow  /deny", "Approval · /allow  /deny")
            .to_owned()
    } else if !app.question.is_empty() {
        app.msg("Risposta richiesta", "Answer requested").to_owned()
    } else if let Some(spinner) = spinner {
        format!("{spinner} {}", app.msg("In elaborazione…", "Working…"))
    } else if !app.plan.is_empty() {
        app.msg("Piano pronto · /build", "Plan ready · /build")
            .to_owned()
    } else {
        string(&app.chat, "state")
    };
    let context = [
        activity,
        app.model.clone(),
        string(&app.chat, "permission_mode"),
    ]
    .into_iter()
    .filter(|part| !part.is_empty())
    .collect::<Vec<_>>()
    .join(" · ");
    paint(
        f,
        status,
        [Line::styled(clean(&context), Style::new().fg(Color::Cyan))],
    );

    let start = (selection + 1).saturating_sub(menu.height as usize);
    let max_item_width = suggestions
        .iter()
        .skip(start)
        .take(menu.height as usize)
        .map(|cmd| ftui::text::display_width(cmd))
        .max()
        .unwrap_or(0)
        .min(usize::from(menu.width).saturating_sub(3));
    for (i, cmd) in suggestions
        .iter()
        .enumerate()
        .skip(start)
        .take(menu.height as usize)
    {
        let (marker, style) = if i == selection {
            ("› ", Style::new().fg(Color::Black).bg(Color::Cyan).bold())
        } else {
            ("  ", Style::default())
        };
        let row = Rect::new(menu.x, menu.y + (i - start) as u16, menu.width, 1);
        let pad = max_item_width.saturating_sub(ftui::text::display_width(cmd)) + 1;
        paint(
            f,
            row,
            [Line::styled(
                format!("{marker}{cmd}{}", " ".repeat(pad)),
                style,
            )],
        );
    }

    let (cursor_row, col) = editor::position(&rows, caret);
    let first = (cursor_row + 1).saturating_sub(input.height as usize);
    let visible = rows
        .iter()
        .enumerate()
        .skip(first)
        .take(input.height as usize)
        .map(|(i, row)| {
            let prompt = if i == 0 { "› " } else { "│ " };
            let value = if app.input.is_empty() {
                Span::styled(
                    app.msg("Scrivi un messaggio…", "Type a message…"),
                    Style::new().dim(),
                )
            } else {
                Span::raw(row.text)
            };
            Line::from_spans(vec![
                Span::styled(prompt, Style::new().fg(Color::Cyan)),
                value,
            ])
        })
        .collect::<Vec<_>>();
    paint(f, input, visible);
    if !input.is_empty() {
        f.set_cursor(Some((
            input.x + (2 + col).min(input.width as usize - 1) as u16,
            input.y + (cursor_row - first) as u16,
        )));
    }
    let hint = if app.scroll > 0 {
        format!(
            "↑ {} · {}",
            app.scroll,
            app.msg("Fine per seguire", "End to follow")
        )
    } else if picker_label.is_some() {
        app.msg(
            "↑↓ scegli · Invio conferma · Esc chiude",
            "↑↓ select · Enter confirm · Esc dismiss",
        )
        .into()
    } else if !suggestions.is_empty() {
        app.msg(
            "↑↓ scegli · Tab completa · Invio esegue · Esc chiude",
            "↑↓ select · Tab complete · Enter run · Esc dismiss",
        )
        .into()
    } else {
        app.msg(
            "Invio invia · F2 conversazioni · F3 progetti · /help",
            "Enter send · F2 conversations · F3 projects · /help",
        )
        .into()
    };
    paint(f, footer, [Line::styled(hint, Style::new().dim())]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assistant_markdown_preserves_visible_text_and_emphasis() {
        clear_markdown_cache();
        let message = Message {
            role: Role::Assistant,
            text: "Use **Venticode** locally.".into(),
        };
        let lines = transcript(0, &message, false, true, 80);
        let line = lines
            .iter()
            .find(|line| line.to_plain_text().contains("Venticode"))
            .unwrap();
        assert_eq!(line.to_plain_text(), "Use Venticode locally.");
        assert!(
            line.spans()
                .iter()
                .any(|span| span.as_str() == "Venticode" && span.style.is_some())
        );
        // Second call hits the cache and returns identical content.
        let cached = transcript(0, &message, false, true, 80);
        assert_eq!(
            cached.iter().map(Line::to_plain_text).collect::<Vec<_>>(),
            lines.iter().map(Line::to_plain_text).collect::<Vec<_>>()
        );
        // Changing text at the same index invalidates the entry.
        let updated = Message {
            role: Role::Assistant,
            text: "Different **text**.".into(),
        };
        let updated_lines = transcript(0, &updated, false, true, 80);
        assert!(
            updated_lines
                .iter()
                .any(|l| l.to_plain_text().contains("Different text."))
        );
    }

    #[test]
    fn preview_limits_wrapped_output_and_expansion_preserves_content() {
        let message = Message {
            role: Role::Output,
            text: "abcdefghijklmnop".into(),
        };
        let render = |compact, en| {
            transcript(0, &message, compact, en, 6)
                .iter()
                .map(Line::to_plain_text)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            render(true, true),
            ["│ abcd", "│ efgh", "│ ijkl", "  … Ctrl+O to expand"]
        );
        assert_eq!(render(true, false)[3], "  … Ctrl+O per espandere");
        assert_eq!(render(false, true)[3], "│ mnop");
    }
}
