//! Ratatui line rendering for interactive A2UI modal forms.

use super::a2ui::{
    A2UIFormState, FocusTarget, FormChoicePicker, FormField, FormItem, FormTextField,
};
use harnx_core::interaction::response::OptionItem;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

pub(super) fn render_form_lines(form: &A2UIFormState, _width: u16) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut rendered_buttons = false;

    for item in &form.items {
        render_form_item(&mut lines, item, form, &mut rendered_buttons);
    }
    if !rendered_buttons && !form.buttons.is_empty() {
        render_buttons_row(&mut lines, form);
    }
    trim_trailing_blank_lines(&mut lines);
    lines
}

fn render_form_item(
    lines: &mut Vec<Line<'static>>,
    item: &FormItem,
    form: &A2UIFormState,
    rendered_buttons: &mut bool,
) {
    match item {
        FormItem::Text { text, caption } => render_text_item(lines, text, *caption),
        FormItem::Field(idx) => {
            let is_focused = form.focus == FocusTarget::Field(*idx);
            if let Some(field) = form.fields.get(*idx) {
                render_field_lines(lines, field, is_focused);
            }
        }
        FormItem::Button(_) => {
            if !*rendered_buttons {
                render_buttons_row(lines, form);
                *rendered_buttons = true;
            }
        }
    }
}

fn render_text_item(lines: &mut Vec<Line<'static>>, text: &str, caption: bool) {
    let style = if caption {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD)
    };
    for part in text.split('\n') {
        lines.push(Line::from(Span::styled(part.to_string(), style)));
    }
    lines.push(Line::from(""));
}

fn trim_trailing_blank_lines(lines: &mut Vec<Line<'static>>) {
    while let Some(last) = lines.last() {
        if last.spans.is_empty() || last.spans.iter().all(|s| s.content.trim().is_empty()) {
            lines.pop();
        } else {
            break;
        }
    }
}

fn render_buttons_row(lines: &mut Vec<Line<'static>>, form: &A2UIFormState) {
    let mut spans = Vec::new();
    for (i, btn) in form.buttons.iter().enumerate() {
        let is_focused = form.focus == FocusTarget::Button(i);
        if !spans.is_empty() {
            spans.push(Span::raw("   "));
        }
        if is_focused {
            spans.push(Span::styled(
                format!("▶ [ {} ] ◀", btn.label),
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ));
        } else if btn.variant.as_deref() == Some("primary") {
            spans.push(Span::styled(
                format!("[ {} ]", btn.label),
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::styled(
                format!("[ {} ]", btn.label),
                Style::default().fg(Color::Yellow),
            ));
        }
    }
    lines.push(Line::from(spans));
    lines.push(Line::from(""));
}

fn render_field_lines(lines: &mut Vec<Line<'static>>, field: &FormField, is_focused: bool) {
    match field {
        FormField::Text(tf) => render_text_field(lines, tf, is_focused),
        FormField::Choice(cp) => render_choice_picker(lines, cp, is_focused),
    }
}

fn render_text_field(lines: &mut Vec<Line<'static>>, tf: &FormTextField, is_focused: bool) {
    lines.push(Line::from(Span::styled(
        format!("{}:", tf.label),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )));
    let mut input_spans = Vec::new();
    let bracket_style = if is_focused {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    input_spans.push(Span::styled("[ ", bracket_style));
    render_input_content(&mut input_spans, tf, is_focused);
    input_spans.push(Span::styled(" ]", bracket_style));
    lines.push(Line::from(input_spans));
    lines.push(Line::from(""));
}

fn render_input_content(spans: &mut Vec<Span<'static>>, tf: &FormTextField, is_focused: bool) {
    if tf.variant.as_deref() == Some("obscured") {
        let masked = "*".repeat(tf.value.len());
        if is_focused {
            render_text_with_cursor(spans, &masked, tf.cursor);
        } else {
            spans.push(Span::styled(masked, Style::default().fg(Color::Reset)));
        }
    } else if tf.value.is_empty() {
        if is_focused {
            spans.push(Span::styled("█", Style::default().fg(Color::Yellow)));
        }
        if let Some(ph) = &tf.placeholder {
            spans.push(Span::styled(
                ph.clone(),
                Style::default().fg(Color::DarkGray),
            ));
        }
    } else if is_focused {
        render_text_with_cursor(spans, &tf.value, tf.cursor);
    } else {
        spans.push(Span::styled(
            tf.value.clone(),
            Style::default().fg(Color::Reset),
        ));
    }
}

fn render_choice_picker(lines: &mut Vec<Line<'static>>, cp: &FormChoicePicker, is_focused: bool) {
    lines.push(Line::from(Span::styled(
        format!("{}:", cp.label),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )));
    for (i, opt) in cp.options.iter().enumerate() {
        let is_selected = cp.selected.iter().any(|v| v == &opt.value);
        let is_hovered = is_focused && cp.highlighted == i;
        render_choice_option(lines, opt, is_selected, is_hovered, cp.exclusive);
    }
    lines.push(Line::from(""));
}

fn render_choice_option(
    lines: &mut Vec<Line<'static>>,
    opt: &OptionItem,
    is_selected: bool,
    is_hovered: bool,
    exclusive: bool,
) {
    let pointer = if is_hovered { "▶ " } else { "  " };
    let icon = if exclusive {
        if is_selected {
            "(*)"
        } else {
            "( )"
        }
    } else if is_selected {
        "[*]"
    } else {
        "[ ]"
    };
    let icon_style = if is_selected {
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let text_style = if is_hovered {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else if is_selected {
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Reset)
    };
    lines.push(Line::from(vec![
        Span::styled(pointer, Style::default().fg(Color::Yellow)),
        Span::styled(format!("{icon} "), icon_style),
        Span::styled(opt.label.clone(), text_style),
    ]));
}

fn render_text_with_cursor(spans: &mut Vec<Span<'static>>, text: &str, cursor: usize) {
    let cursor = cursor.min(text.len());
    let before = &text[..cursor];
    if !before.is_empty() {
        spans.push(Span::styled(
            before.to_string(),
            Style::default().fg(Color::Reset),
        ));
    }
    if cursor < text.len() {
        let char_len = text[cursor..].chars().next().map_or(1, |c| c.len_utf8());
        let ch = &text[cursor..cursor + char_len];
        spans.push(Span::styled(
            ch.to_string(),
            Style::default().add_modifier(Modifier::REVERSED),
        ));
        let after = &text[cursor + char_len..];
        if !after.is_empty() {
            spans.push(Span::styled(
                after.to_string(),
                Style::default().fg(Color::Reset),
            ));
        }
    } else {
        spans.push(Span::styled("█", Style::default().fg(Color::Yellow)));
    }
}

pub(super) fn footer_help(interrupt: &str) -> String {
    format!("Tab navigate · Space select · Enter activate · Ctrl+D deny · Ctrl+C {interrupt}")
}
