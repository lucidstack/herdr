use super::*;
use crate::api::schema::WorkItemStepStatus;

use super::super::super::work_items::{
    is_hidden, ClientInboxOverlay, ClientWorkItemOverlay, SPINNER_FRAMES,
};

const WIDTH: u16 = 80;

pub(super) fn render_work_item_overlay(
    b: &mut Buffer,
    o: &ClientWorkItemOverlay,
    p: &Palette,
) -> Option<OverlayRender> {
    if o.checklist() {
        render_checklist(b, o, p)
    } else {
        render_choices(b, o, p)
    }
}

/// Longest the status message above the choices may wrap to.
const MAX_STATUS_ROWS: usize = 3;
/// Most switches the dialog shows for one choice: each has a digit to flip it.
const MAX_OPTION_ROWS: usize = 9;

fn render_choices(b: &mut Buffer, o: &ClientWorkItemOverlay, p: &Palette) -> Option<OverlayRender> {
    let item = &o.item;
    let choice_count = item.choices.len() as u16;
    let (status, status_color) = status_message(item, p);
    let status_rows = status.map_or(Vec::new(), |message| {
        wrap(
            &message,
            usize::from(WIDTH.saturating_sub(4)),
            MAX_STATUS_ROWS,
        )
    });
    let status_height = status_rows.len().max(1) as u16;
    // Rows kept free below the choices for the most switches any choice has, so the dialog
    // keeps its height as the highlight moves.
    let option_rows = item
        .choices
        .iter()
        .map(|choice| choice.options.len())
        .max()
        .unwrap_or(0)
        .min(MAX_OPTION_ROWS) as u16;
    // Heading, title and status, gap, one row per choice, the switches, gap, detail, hint,
    // plus borders.
    let q = popup(
        b.area,
        WIDTH,
        choice_count + 8 + status_height + option_rows,
    )?;
    let i = panel(b, q, p.accent, p.panel_bg)?;
    let mut heading = format!(" {}", item.context);
    if let Some(state) = &item.tracker_state {
        heading.push_str(&format!(" · {state}"));
    }
    if let Some(pull_request) = &item.own_pull_request {
        heading.push_str(&format!(" · {}", pull_request.status));
    }
    if let Some(author) = &item.author {
        heading.push_str(&format!(" · @{author}"));
    }
    put_text(
        b,
        i.x,
        i.y,
        i.width,
        &heading,
        Style::default()
            .fg(p.accent)
            .bg(p.panel_bg)
            .add_modifier(Modifier::BOLD),
    );
    put_text(
        b,
        i.x,
        i.y + 1,
        i.width,
        &format!(" {}", item.title),
        Style::default().fg(p.text).bg(p.panel_bg),
    );
    for (row, line) in status_rows.iter().enumerate() {
        put_text(
            b,
            i.x,
            i.y + 2 + row as u16,
            i.width,
            &format!(" {line}"),
            Style::default().fg(status_color).bg(p.panel_bg),
        );
    }
    let succeeded = item
        .action_outcome
        .as_ref()
        .filter(|outcome| outcome.succeeded)
        .map(|outcome| outcome.choice_id.as_str());
    let spinner = SPINNER_FRAMES[o.spinner_frame % SPINNER_FRAMES.len()];
    let options_top = i.bottom().saturating_sub(3 + option_rows);
    let mut menu_rows = Vec::with_capacity(item.choices.len());
    for (index, choice) in item.choices.iter().enumerate() {
        let y = i.y + 3 + status_height + index as u16;
        if y >= options_top {
            break;
        }
        let rect = Rect::new(i.x + 1, y, i.width.saturating_sub(2), 1);
        let highlighted = index == o.highlighted;
        let done = succeeded == Some(choice.choice_id.as_str());
        let running = item.running_choice_id.as_deref() == Some(choice.choice_id.as_str());
        let unavailable = choice.disabled_reason.is_some() || done;
        let style = if highlighted && !unavailable {
            Style::default()
                .fg(contrast(p))
                .bg(p.accent)
                .add_modifier(Modifier::BOLD)
        } else if highlighted {
            Style::default().fg(p.overlay0).bg(p.surface0)
        } else if unavailable {
            Style::default().fg(p.overlay0).bg(p.panel_bg)
        } else {
            Style::default().fg(p.text).bg(p.panel_bg)
        };
        b.set_style(rect, style);
        let marker = if running {
            spinner
        } else if highlighted {
            "↵"
        } else {
            " "
        };
        let x = put_segment(
            b,
            rect.x,
            rect.y,
            rect.right(),
            &format!(" {marker} {}", choice.label),
            style,
        );
        if done {
            put_segment(b, x, rect.y, rect.right(), " ✓", style.fg(p.green));
        }
        menu_rows.push((rect, index));
    }
    // The highlighted choice's switches, flipped with their number or a click.
    let highlighted_options = item
        .choices
        .get(o.highlighted)
        .filter(|choice| choice.disabled_reason.is_none());
    let mut option_hits = Vec::new();
    if let Some(choice) = highlighted_options {
        for (index, option) in choice.options.iter().take(MAX_OPTION_ROWS).enumerate() {
            let y = options_top + index as u16;
            if y >= i.bottom().saturating_sub(3) {
                break;
            }
            let rect = Rect::new(i.x + 1, y, i.width.saturating_sub(2), 1);
            let on = o.option_on(choice, option);
            let style = Style::default()
                .fg(if on { p.text } else { p.subtext0 })
                .bg(p.panel_bg);
            put_text(
                b,
                rect.x,
                rect.y,
                rect.width,
                &format!(
                    "   {} [{}] {}",
                    index + 1,
                    if on { "x" } else { " " },
                    option.label
                ),
                style,
            );
            option_hits.push((rect, index));
        }
    }
    let confirming = o
        .confirming
        .filter(|index| *index == o.highlighted)
        .and_then(|index| item.choices.get(index))
        .and_then(|choice| choice.confirm.as_ref());
    if let Some(choice) = item.choices.get(o.highlighted) {
        let (detail, color) = match (confirming, &choice.disabled_reason, &choice.description) {
            (Some(prompt), _, _) => (Some(prompt), p.peach),
            (None, Some(reason), _) => (Some(reason), p.peach),
            (None, None, Some(description)) => (Some(description), p.subtext0),
            (None, None, None) => (None, p.subtext0),
        };
        if let Some(detail) = detail {
            put_text(
                b,
                i.x,
                i.bottom().saturating_sub(2),
                i.width,
                &format!(" {detail}"),
                Style::default().fg(color).bg(p.panel_bg),
            );
        }
    }
    let toggles = highlighted_options
        .map(|choice| choice.options.len().min(MAX_OPTION_ROWS))
        .filter(|count| *count > 0);
    let hint = match (confirming.is_some(), toggles) {
        (true, _) => " ↵ again to confirm · ↑/↓ choose · esc cancel".to_string(),
        (false, Some(1)) => " ↑/↓ choose · 1 toggle · ↵ confirm · esc cancel".to_string(),
        (false, Some(count)) => {
            format!(" ↑/↓ choose · 1-{count} toggle · ↵ confirm · esc cancel")
        }
        (false, None) => " ↑/↓ choose · ↵ confirm · esc cancel".to_string(),
    };
    put_text(
        b,
        i.x,
        i.bottom().saturating_sub(1),
        i.width,
        &hint,
        Style::default().fg(p.overlay0).bg(p.panel_bg),
    );
    Some(OverlayRender {
        area: q,
        primary: menu_rows
            .iter()
            .find(|(_, index)| *index == o.highlighted)
            .map(|(rect, _)| *rect)
            .unwrap_or_default(),
        cancel: Rect::default(),
        menu_rows,
        option_rows: option_hits,
        ..OverlayRender::default()
    })
}

/// The line under the title: how the last action ended, else the item's notice, reminder
/// or summary.
fn status_message<'a>(
    item: &'a crate::api::schema::WorkItemInfo,
    p: &Palette,
) -> (Option<std::borrow::Cow<'a, str>>, ratatui::style::Color) {
    if item.running_choice_id.is_none() {
        if let Some(outcome) = &item.action_outcome {
            return if outcome.succeeded {
                (Some(format!("✓ {}", outcome.message).into()), p.green)
            } else {
                (Some(outcome.message.as_str().into()), p.red)
            };
        }
    }
    if let Some(notice) = &item.notice {
        (Some(notice.as_str().into()), p.peach)
    } else if let Some(reminder) = &item.start_reminder {
        (Some(reminder.as_str().into()), p.yellow)
    } else if let Some(summary) = &item.summary {
        (Some(summary.as_str().into()), p.subtext0)
    } else {
        (None, p.subtext0)
    }
}

/// Word-wraps `text` to `width` columns in at most `max_rows` rows; the last row keeps the
/// rest and is cut when drawn.
fn wrap(text: &str, width: usize, max_rows: usize) -> Vec<String> {
    use unicode_width::UnicodeWidthStr;

    let mut rows: Vec<String> = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        let needed = current.width() + usize::from(!current.is_empty()) + word.width();
        if !current.is_empty() && needed > width && rows.len() + 1 < max_rows {
            rows.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        rows.push(current);
    }
    rows
}

fn render_checklist(
    b: &mut Buffer,
    o: &ClientWorkItemOverlay,
    p: &Palette,
) -> Option<OverlayRender> {
    let steps = o
        .item
        .provisioning
        .as_ref()
        .map(|provisioning| provisioning.steps.as_slice())
        .unwrap_or(&[]);
    let step_rows = steps.len().max(1) as u16;
    let q = popup(b.area, WIDTH, step_rows + 5)?;
    let i = panel(b, q, p.accent, p.panel_bg)?;
    put_text(
        b,
        i.x,
        i.y,
        i.width,
        &format!(" Preparing {}", o.item.context),
        Style::default()
            .fg(p.accent)
            .bg(p.panel_bg)
            .add_modifier(Modifier::BOLD),
    );
    let spinner = SPINNER_FRAMES[o.spinner_frame % SPINNER_FRAMES.len()];
    if steps.is_empty() {
        put_text(
            b,
            i.x,
            i.y + 1,
            i.width,
            &format!(" {spinner} Starting…"),
            Style::default().fg(p.text).bg(p.panel_bg),
        );
    }
    for (index, step) in steps.iter().enumerate() {
        let y = i.y + 1 + index as u16;
        let (glyph, color) = match step.status {
            WorkItemStepStatus::Pending => ("○", p.overlay0),
            WorkItemStepStatus::Running => (spinner, p.yellow),
            WorkItemStepStatus::Done => ("✓", p.green),
            WorkItemStepStatus::Skipped => ("–", p.overlay0),
            WorkItemStepStatus::Failed => ("✗", p.red),
            WorkItemStepStatus::Unknown => ("?", p.overlay0),
        };
        let x = put_segment(
            b,
            i.x + 1,
            y,
            i.right(),
            glyph,
            Style::default().fg(color).bg(p.panel_bg),
        );
        let x = put_segment(
            b,
            x.saturating_add(1),
            y,
            i.right(),
            &step.label,
            Style::default().fg(p.text).bg(p.panel_bg),
        );
        if let Some(detail) = &step.detail {
            put_segment(
                b,
                x,
                y,
                i.right(),
                &format!(" — {detail}"),
                Style::default().fg(p.overlay0).bg(p.panel_bg),
            );
        }
    }
    let open_label = " ↵ open workspace ";
    let back_label = format!(" esc back to {} ", o.return_label);
    let rects = row(
        i,
        &[display_width(open_label), display_width(&back_label)],
        2,
        i.height.saturating_sub(1),
    );
    let [open, back] = rects.as_slice() else {
        return None;
    };
    let open_style = if o.item.workspace_id.is_some() {
        Style::default()
            .fg(contrast(p))
            .bg(p.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(p.overlay0).bg(p.surface0)
    };
    button(b, *open, open_label, open_style);
    button(
        b,
        *back,
        &back_label,
        Style::default()
            .fg(p.text)
            .bg(p.surface0)
            .add_modifier(Modifier::BOLD),
    );
    Some(OverlayRender {
        area: q,
        primary: *open,
        cancel: *back,
        ..OverlayRender::default()
    })
}

/// Keyboard list of every item, hidden ones last.
pub(super) fn render_inbox_overlay(
    b: &mut Buffer,
    o: &ClientInboxOverlay,
    p: &Palette,
) -> Option<OverlayRender> {
    let max_rows = b.area.height.saturating_sub(8).max(1);
    let list_rows = (o.items.len().max(1) as u16).min(max_rows);
    let q = popup(b.area, WIDTH, list_rows + 4)?;
    let i = panel(b, q, p.accent, p.panel_bg)?;
    put_text(
        b,
        i.x,
        i.y,
        i.width,
        " Inbox",
        Style::default()
            .fg(p.accent)
            .bg(p.panel_bg)
            .add_modifier(Modifier::BOLD),
    );
    if o.items.is_empty() {
        put_text(
            b,
            i.x,
            i.y + 1,
            i.width,
            " Nothing needs you right now",
            Style::default().fg(p.subtext0).bg(p.panel_bg),
        );
    }
    // Keep the highlighted row in view.
    let first = o
        .highlighted
        .saturating_sub(usize::from(list_rows).saturating_sub(1));
    let mut menu_rows = Vec::new();
    for (row, (index, item)) in o
        .items
        .iter()
        .enumerate()
        .skip(first)
        .take(usize::from(list_rows))
        .enumerate()
    {
        let y = i.y + 1 + row as u16;
        let rect = Rect::new(i.x, y, i.width, 1);
        let highlighted = index == o.highlighted;
        let hidden = is_hidden(item);
        let base = if highlighted {
            Style::default().fg(contrast(p)).bg(p.accent)
        } else {
            Style::default()
                .fg(if hidden { p.overlay0 } else { p.text })
                .bg(p.panel_bg)
        };
        b.set_style(rect, base);
        let marker = if !item.seen {
            "●"
        } else if item.resolved {
            "✓"
        } else {
            "·"
        };
        let x = put_segment(b, i.x + 1, y, i.right(), marker, base);
        let x = put_segment(
            b,
            x.saturating_add(1),
            y,
            i.right(),
            &item.context,
            base.add_modifier(Modifier::BOLD),
        );
        let state = if item.dismissed {
            "  dismissed"
        } else if item.snoozed_until.is_some() {
            "  snoozed"
        } else {
            ""
        };
        let title_end = i.right().saturating_sub(display_width(state) + 1);
        put_segment(b, x, y, title_end, &format!("  {}", item.title), base);
        if !state.is_empty() {
            put_segment(b, title_end, y, i.right(), state, base);
        }
        menu_rows.push((rect, index));
    }
    put_text(
        b,
        i.x,
        i.bottom().saturating_sub(1),
        i.width,
        " ↑/↓ select · ↵ open · m more · d dismiss · s snooze 1 h · u show again · esc close",
        Style::default().fg(p.overlay0).bg(p.panel_bg),
    );
    Some(OverlayRender {
        area: q,
        menu_rows,
        ..OverlayRender::default()
    })
}
