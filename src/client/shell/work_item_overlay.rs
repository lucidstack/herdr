use super::*;
use crate::api::schema::WorkItemStepStatus;

use super::super::super::work_items::{ClientWorkItemOverlay, SPINNER_FRAMES};

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

fn render_choices(b: &mut Buffer, o: &ClientWorkItemOverlay, p: &Palette) -> Option<OverlayRender> {
    let item = &o.item;
    let choice_count = item.choices.len() as u16;
    // Heading (3 rows), gap, one row per choice, gap, detail, hint, plus borders.
    let q = popup(b.area, WIDTH, choice_count + 9)?;
    let i = panel(b, q, p.accent, p.panel_bg)?;
    let heading = match &item.author {
        Some(author) => format!(" {} · @{author}", item.context),
        None => format!(" {}", item.context),
    };
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
    if let Some(notice) = &item.notice {
        put_text(
            b,
            i.x,
            i.y + 2,
            i.width,
            &format!(" {notice}"),
            Style::default().fg(p.peach).bg(p.panel_bg),
        );
    } else if let Some(summary) = &item.summary {
        put_text(
            b,
            i.x,
            i.y + 2,
            i.width,
            &format!(" {summary}"),
            Style::default().fg(p.subtext0).bg(p.panel_bg),
        );
    }
    let mut menu_rows = Vec::with_capacity(item.choices.len());
    for (index, choice) in item.choices.iter().enumerate() {
        let y = i.y + 4 + index as u16;
        if y >= i.bottom().saturating_sub(3) {
            break;
        }
        let rect = Rect::new(i.x + 1, y, i.width.saturating_sub(2), 1);
        let highlighted = index == o.highlighted;
        let style = if highlighted && choice.disabled_reason.is_none() {
            Style::default()
                .fg(contrast(p))
                .bg(p.accent)
                .add_modifier(Modifier::BOLD)
        } else if highlighted {
            Style::default().fg(p.overlay0).bg(p.surface0)
        } else if choice.disabled_reason.is_some() {
            Style::default().fg(p.overlay0).bg(p.panel_bg)
        } else {
            Style::default().fg(p.text).bg(p.panel_bg)
        };
        b.set_style(rect, style);
        let marker = if highlighted { "↵" } else { " " };
        put_text(
            b,
            rect.x,
            rect.y,
            rect.width,
            &format!(" {marker} {}", choice.label),
            style,
        );
        menu_rows.push((rect, index));
    }
    if let Some(choice) = item.choices.get(o.highlighted) {
        let (detail, color) = match (&choice.disabled_reason, &choice.description) {
            (Some(reason), _) => (Some(reason), p.peach),
            (None, Some(description)) => (Some(description), p.subtext0),
            (None, None) => (None, p.subtext0),
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
    put_text(
        b,
        i.x,
        i.bottom().saturating_sub(1),
        i.width,
        " ↑/↓ choose · ↵ confirm · esc cancel",
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
        ..OverlayRender::default()
    })
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
