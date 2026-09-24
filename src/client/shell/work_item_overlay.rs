use super::*;
use crate::api::schema::WorkItemStepStatus;

use super::super::super::work_items::{ClientWorkItemOverlay, SPINNER_FRAMES};

const WIDTH: u16 = 64;

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
    let q = popup(b.area, WIDTH, 8)?;
    let i = panel(b, q, p.accent, p.panel_bg)?;
    let item = &o.item;
    put_text(
        b,
        i.x,
        i.y,
        i.width,
        &format!(" {}", item.context),
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
    let widths: Vec<u16> = item
        .choices
        .iter()
        .map(|choice| display_width(&choice.label).saturating_add(4))
        .collect();
    let rects = row(i, &widths, 2, 3);
    let mut menu_rows = Vec::with_capacity(rects.len());
    for (index, (choice, rect)) in item.choices.iter().zip(&rects).enumerate() {
        let highlighted = index == o.highlighted;
        let (text, style) = if choice.disabled_reason.is_some() {
            (
                format!(" {} ", choice.label),
                Style::default().fg(p.overlay0).bg(p.surface0),
            )
        } else if highlighted {
            (
                format!(" ↵ {} ", choice.label),
                Style::default()
                    .fg(contrast(p))
                    .bg(p.accent)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            (
                format!(" {} ", choice.label),
                Style::default().fg(p.text).bg(p.surface0),
            )
        };
        button(b, *rect, &text, style);
        menu_rows.push((*rect, index));
    }
    if let Some(reason) = item
        .choices
        .get(o.highlighted)
        .and_then(|choice| choice.disabled_reason.as_deref())
    {
        put_text(
            b,
            i.x,
            i.y + 4,
            i.width,
            &format!(" {reason}"),
            Style::default().fg(p.overlay0).bg(p.panel_bg),
        );
    }
    put_text(
        b,
        i.x,
        i.bottom().saturating_sub(1),
        i.width,
        " ←/→ choose · ↵ confirm · esc cancel",
        Style::default().fg(p.overlay0).bg(p.panel_bg),
    );
    Some(OverlayRender {
        area: q,
        primary: rects.get(o.highlighted).copied().unwrap_or_default(),
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
