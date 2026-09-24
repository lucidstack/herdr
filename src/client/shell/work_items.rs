//! Client-side work items: the per-endpoint projection copy, the sidebar
//! inbox section, and the work-item dialog's state and input.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use super::*;
use crate::api::schema::{
    Method, WorkItemChoiceAction, WorkItemChooseParams, WorkItemInfo, WorkItemPhase,
    WorkItemStepStatus, WorkItemTarget, WorkspaceTarget,
};
use crate::client::endpoint::ClientEndpointId;
use crate::protocol::work_items::EndpointWorkItemsProjection;

use super::render::{put_right_text, put_segment, put_text};

pub(super) const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const SPINNER_INTERVAL: Duration = Duration::from_millis(200);

#[derive(Default)]
pub(crate) struct ClientWorkItems {
    by_endpoint: HashMap<ClientEndpointId, EndpointWorkItemsProjection>,
    pub(super) spinner_frame: usize,
    spinner_last_tick: Option<Instant>,
}

impl ClientWorkItems {
    pub(super) fn remove_endpoint(&mut self, endpoint_id: &ClientEndpointId) {
        self.by_endpoint.remove(endpoint_id);
    }

    /// Stores `projection` when it is newer than the current one; returns whether it was stored.
    fn store(
        &mut self,
        endpoint_id: &ClientEndpointId,
        projection: EndpointWorkItemsProjection,
    ) -> bool {
        let newer = self.by_endpoint.get(endpoint_id).is_none_or(|current| {
            current.boot_id != projection.boot_id || projection.revision > current.revision
        });
        if newer {
            self.by_endpoint.insert(endpoint_id.clone(), projection);
        }
        newer
    }
}

pub(super) struct WorkItemHit {
    pub(super) rect: Rect,
    pub(super) item_id: String,
}

#[derive(Debug)]
pub(super) struct ClientWorkItemOverlay {
    pub(super) item: WorkItemInfo,
    pub(super) highlighted: usize,
    pub(super) return_workspace_id: Option<String>,
    pub(super) return_label: String,
    /// The user confirmed local review and the server has not reported progress yet.
    pub(super) awaiting_provisioning: bool,
    /// Shows provisioning progress instead of the choices.
    pub(super) show_checklist: bool,
    pub(super) spinner_frame: usize,
}

impl ClientWorkItemOverlay {
    pub(super) fn checklist(&self) -> bool {
        self.show_checklist
    }
}

/// The Local projection belonging to the current snapshot, when a source is configured.
pub(super) fn local_projection<'a>(
    items: &'a ClientWorkItems,
    snapshot: &ClientShellSnapshot,
) -> Option<&'a EndpointWorkItemsProjection> {
    items
        .by_endpoint
        .get(&ClientEndpointId::Local)
        .filter(|projection| {
            projection.boot_id == snapshot.boot_id && !projection.sources.is_empty()
        })
}

fn provisioning_running(item: &WorkItemInfo) -> bool {
    item.provisioning
        .as_ref()
        .is_some_and(|provisioning| !provisioning.finished)
}

fn provisioning_failed(item: &WorkItemInfo) -> bool {
    item.provisioning.as_ref().is_some_and(|provisioning| {
        provisioning.finished
            && provisioning
                .steps
                .iter()
                .any(|step| step.status == WorkItemStepStatus::Failed)
    })
}

fn item_animates(item: &WorkItemInfo) -> bool {
    item.phase == WorkItemPhase::AwaitingExternal || provisioning_running(item)
}

/// Renders the inbox section at the top of `area`. Returns the rows it used and the
/// workspaces drawn nested under their items; only those leave the spaces list, so a
/// workspace whose item overflowed stays reachable.
pub(super) fn render_items_section<'a>(
    buffer: &mut Buffer,
    area: Rect,
    projection: &'a EndpointWorkItemsProjection,
    snapshot: &ClientShellSnapshot,
    config: &ClientShellConfig,
    spinner_frame: usize,
    hits: &mut ShellHitMap,
) -> (u16, Vec<&'a str>) {
    let mut nested_workspace_ids = Vec::new();
    if area.is_empty() {
        return (0, nested_workspace_ids);
    }
    let palette = &config.palette;
    let limit = area
        .y
        .saturating_add(3.max(area.height / 2).min(area.height));
    let mut y = area.y;
    put_text(
        buffer,
        area.x,
        y,
        area.width,
        " inbox",
        Style::default()
            .fg(palette.overlay0)
            .add_modifier(Modifier::BOLD),
    );
    let unseen = projection.items.iter().filter(|item| !item.seen).count();
    if unseen > 0 {
        put_right_text(
            buffer,
            Rect::new(area.x, y, area.width.saturating_sub(1), 1),
            y,
            &format!("{unseen} new "),
            Style::default().fg(palette.teal),
        );
    } else if projection.items.is_empty() {
        put_right_text(
            buffer,
            Rect::new(area.x, y, area.width.saturating_sub(1), 1),
            y,
            "none ",
            Style::default().fg(palette.overlay0),
        );
    }
    y += 1;
    for source in &projection.sources {
        let Some(error) = &source.error else {
            continue;
        };
        if y >= limit {
            break;
        }
        put_text(
            buffer,
            area.x,
            y,
            area.width.saturating_sub(1),
            &format!(" ! {}: {error}", source.label),
            Style::default().fg(palette.peach),
        );
        y += 1;
    }

    let focused_workspace_id = snapshot.focused_workspace_id.as_deref();
    for (index, item) in projection.items.iter().enumerate() {
        let nested = item.workspace_id.as_deref().and_then(|workspace_id| {
            snapshot
                .workspaces
                .iter()
                .enumerate()
                .find(|(_, workspace)| workspace.workspace_id == workspace_id)
        });
        let nested_rows = nested.map(|(_, workspace)| {
            super::render::sidebar::workspace_rows(
                workspace,
                workspace.agent_status,
                true,
                &config.spaces,
            )
        });
        let nested_height = nested_rows
            .as_ref()
            .map_or(0, |rows| rows.len().max(1) as u16);
        let remaining = projection.items.len() - index;
        // Keep one row for the "more" marker unless this is the last item.
        let reserve = u16::from(remaining > 1);
        if y.saturating_add(2 + nested_height + reserve) > limit {
            put_text(
                buffer,
                area.x,
                y.min(limit.saturating_sub(1)),
                area.width.saturating_sub(1),
                &format!(" +{remaining} more"),
                Style::default().fg(palette.overlay0),
            );
            y = y.saturating_add(1).min(limit);
            break;
        }
        let item_rect = Rect::new(area.x, y, area.width.saturating_sub(1), 2);
        render_item_rows(
            buffer,
            item_rect,
            item,
            focused_workspace_id,
            spinner_frame,
            palette,
        );
        hits.work_items.push(WorkItemHit {
            rect: item_rect,
            item_id: item.item_id.clone(),
        });
        y += 2;
        if let (Some((workspace_index, workspace)), Some(rows)) = (nested, nested_rows) {
            let rect = Rect::new(area.x, y, area.width.saturating_sub(1), nested_height);
            let selected = false;
            if workspace.focused {
                buffer.set_style(rect, Style::default().bg(palette.active_row_bg));
            }
            super::render::sidebar::render_workspace_rows(
                buffer,
                rect,
                workspace,
                workspace.agent_status,
                config.status_indicators,
                &WorkspaceEntry {
                    index: workspace_index,
                    indented: true,
                    last_child: true,
                },
                rows,
                true,
                selected,
                false,
                palette,
            );
            hits.workspaces.push(WorkspaceHit {
                rect,
                endpoint_id: ClientEndpointId::Local,
                workspace_id: workspace.workspace_id.clone(),
                indented: true,
                group_toggle: None,
            });
            if let Some(workspace_id) = item.workspace_id.as_deref() {
                nested_workspace_ids.push(workspace_id);
            }
            y += nested_height;
        }
    }
    // Blank separator before the spaces list.
    ((y + 1).min(area.bottom()) - area.y, nested_workspace_ids)
}

fn render_item_rows(
    buffer: &mut Buffer,
    rect: Rect,
    item: &WorkItemInfo,
    focused_workspace_id: Option<&str>,
    spinner_frame: usize,
    palette: &Palette,
) {
    if item.workspace_id.is_some() && item.workspace_id.as_deref() == focused_workspace_id {
        buffer.set_style(rect, Style::default().bg(palette.active_row_bg));
    }
    let (marker, marker_color) = if !item.seen {
        ("●", palette.teal)
    } else if item.resolved {
        ("✓", palette.green)
    } else {
        ("·", palette.overlay0)
    };
    let x = put_segment(buffer, rect.x, rect.y, rect.right(), " ", Style::default());
    let x = put_segment(
        buffer,
        x,
        rect.y,
        rect.right(),
        marker,
        Style::default().fg(marker_color),
    );
    let status = if item_animates(item) {
        Some((
            SPINNER_FRAMES[spinner_frame % SPINNER_FRAMES.len()],
            palette.yellow,
        ))
    } else if provisioning_failed(item) {
        Some(("✗", palette.red))
    } else {
        None
    };
    let status_width = u16::from(status.is_some()) * 2;
    let context_style = if item.seen {
        Style::default().fg(palette.mauve)
    } else {
        Style::default()
            .fg(palette.mauve)
            .add_modifier(Modifier::BOLD)
    };
    put_segment(
        buffer,
        x.saturating_add(1),
        rect.y,
        rect.right().saturating_sub(status_width),
        &item.context,
        context_style,
    );
    if let Some((glyph, color)) = status {
        put_right_text(
            buffer,
            Rect::new(rect.x, rect.y, rect.width.saturating_sub(1), 1),
            rect.y,
            glyph,
            Style::default().fg(color),
        );
    }
    let title_y = rect.y.saturating_add(1);
    let x = put_segment(
        buffer,
        rect.x.saturating_add(3),
        title_y,
        rect.right(),
        &item.title,
        Style::default().fg(if item.seen {
            palette.subtext0
        } else {
            palette.text
        }),
    );
    if let Some(author) = &item.author {
        put_segment(
            buffer,
            x,
            title_y,
            rect.right(),
            &format!(" · @{author}"),
            Style::default().fg(palette.overlay0),
        );
    }
}

impl ClientShellState {
    pub(crate) fn set_endpoint_work_items(
        &mut self,
        endpoint_id: &ClientEndpointId,
        projection: EndpointWorkItemsProjection,
    ) -> bool {
        if !self.work_items.store(endpoint_id, projection) {
            return false;
        }
        if endpoint_id.is_local() {
            self.refresh_work_item_overlay();
        }
        true
    }

    fn local_work_item(&self, item_id: &str) -> Option<&WorkItemInfo> {
        let snapshot = self.snapshot.as_deref()?;
        local_projection(&self.work_items, snapshot)?
            .items
            .iter()
            .find(|item| item.item_id == item_id)
    }

    fn refresh_work_item_overlay(&mut self) {
        let Some(ClientShellOverlay::WorkItem(overlay)) = self.overlay.as_ref() else {
            return;
        };
        let Some(item) = self.local_work_item(&overlay.item.item_id).cloned() else {
            self.overlay = None;
            return;
        };
        let Some(ClientShellOverlay::WorkItem(overlay)) = self.overlay.as_mut() else {
            return;
        };
        if item.provisioning.is_some() {
            overlay.awaiting_provisioning = false;
        }
        overlay.highlighted = overlay
            .highlighted
            .min(item.choices.len().saturating_sub(1));
        overlay.item = item;
    }

    /// Advances the spinner while something animates. Returns whether to repaint.
    pub(crate) fn tick_work_items(&mut self, now: Instant) -> bool {
        let sidebar_animates = self
            .snapshot
            .as_deref()
            .and_then(|snapshot| local_projection(&self.work_items, snapshot))
            .is_some_and(|projection| projection.items.iter().any(item_animates));
        let overlay_animates = match self.overlay.as_ref() {
            Some(ClientShellOverlay::WorkItem(overlay)) => {
                (overlay.awaiting_provisioning && overlay.item.provisioning.is_none())
                    || overlay
                        .item
                        .provisioning
                        .as_ref()
                        .is_some_and(|provisioning| {
                            provisioning
                                .steps
                                .iter()
                                .any(|step| step.status == WorkItemStepStatus::Running)
                        })
            }
            _ => false,
        };
        if !sidebar_animates && !overlay_animates {
            self.work_items.spinner_last_tick = None;
            return false;
        }
        if self
            .work_items
            .spinner_last_tick
            .is_some_and(|last| now.saturating_duration_since(last) < SPINNER_INTERVAL)
        {
            return false;
        }
        self.work_items.spinner_last_tick = Some(now);
        self.work_items.spinner_frame = self.work_items.spinner_frame.wrapping_add(1);
        if let Some(ClientShellOverlay::WorkItem(overlay)) = self.overlay.as_mut() {
            overlay.spinner_frame = self.work_items.spinner_frame;
        }
        true
    }

    fn open_work_item_overlay(&mut self, item: WorkItemInfo, show_checklist: bool) {
        let snapshot = self.snapshot.as_deref();
        let return_workspace_id =
            snapshot.and_then(|snapshot| snapshot.focused_workspace_id.clone());
        let return_label = snapshot
            .zip(return_workspace_id.as_deref())
            .and_then(|(snapshot, workspace_id)| {
                snapshot
                    .workspaces
                    .iter()
                    .find(|workspace| workspace.workspace_id == workspace_id)
            })
            .map_or_else(
                || "current workspace".to_owned(),
                |workspace| workspace.label.clone(),
            );
        let highlighted = item
            .default_choice_id
            .as_deref()
            .and_then(|default| {
                item.choices
                    .iter()
                    .position(|choice| choice.choice_id == default)
            })
            .unwrap_or(0);
        self.overlay = Some(ClientShellOverlay::WorkItem(Box::new(
            ClientWorkItemOverlay {
                item,
                highlighted,
                return_workspace_id,
                return_label,
                awaiting_provisioning: false,
                show_checklist,
                spinner_frame: self.work_items.spinner_frame,
            },
        )));
    }

    fn workspace_in_snapshot(&self, workspace_id: &str) -> bool {
        self.snapshot.as_deref().is_some_and(|snapshot| {
            snapshot
                .workspaces
                .iter()
                .any(|workspace| workspace.workspace_id == workspace_id)
        })
    }

    pub(super) fn handle_work_item_click(
        &mut self,
        point: (u16, u16),
        outcome: &mut ClientShellInput,
    ) -> bool {
        let Some(item_id) = self
            .hits
            .work_items
            .iter()
            .find(|hit| super::contains(hit.rect, point))
            .map(|hit| hit.item_id.clone())
        else {
            return false;
        };
        let Some(item) = self.local_work_item(&item_id).cloned() else {
            return false;
        };
        outcome.repaint = true;
        if !item.seen {
            self.push_endpoint_method(
                Method::WorkItemMarkSeen(WorkItemTarget {
                    item_id: item.item_id.clone(),
                }),
                outcome,
            );
        }
        if provisioning_running(&item) {
            self.open_work_item_overlay(item, true);
        } else if let Some(workspace_id) = item
            .workspace_id
            .clone()
            .filter(|workspace_id| self.workspace_in_snapshot(workspace_id))
        {
            self.push_endpoint_method(
                Method::WorkspaceFocus(WorkspaceTarget { workspace_id }),
                outcome,
            );
        } else {
            self.open_work_item_overlay(item, false);
        }
        true
    }

    /// Handles keys while the work-item dialog is open. Returns false for other overlays.
    pub(super) fn route_work_item_overlay_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let Some(ClientShellOverlay::WorkItem(overlay)) = self.overlay.as_mut() else {
            return false;
        };
        outcome.repaint = true;
        if overlay.checklist() {
            match key.code {
                KeyCode::Enter => self.open_work_item_workspace(outcome),
                KeyCode::Esc => self.leave_work_item_checklist(outcome),
                _ => {}
            }
            return true;
        }
        match key.code {
            KeyCode::Left | KeyCode::Up | KeyCode::BackTab | KeyCode::Char('h' | 'k') => {
                move_highlight(overlay, -1)
            }
            KeyCode::Right | KeyCode::Down | KeyCode::Tab | KeyCode::Char('l' | 'j') => {
                move_highlight(overlay, 1)
            }
            KeyCode::Enter => {
                let index = overlay.highlighted;
                self.confirm_work_item_choice(index, outcome);
            }
            KeyCode::Esc => self.overlay = None,
            _ => {}
        }
        true
    }

    pub(super) fn handle_work_item_overlay_click(
        &mut self,
        point: (u16, u16),
        outcome: &mut ClientShellInput,
    ) {
        let Some(ClientShellOverlay::WorkItem(overlay)) = self.overlay.as_ref() else {
            return;
        };
        outcome.repaint = true;
        let checklist = overlay.checklist();
        if let Some(index) = self
            .hits
            .overlay_choice_rows
            .iter()
            .find(|(rect, _)| super::contains(*rect, point))
            .map(|(_, index)| *index)
            .filter(|_| !checklist)
        {
            self.confirm_work_item_choice(index, outcome);
        } else if super::contains(self.hits.overlay_primary, point) {
            if checklist {
                self.open_work_item_workspace(outcome);
            } else {
                let index = overlay.highlighted;
                self.confirm_work_item_choice(index, outcome);
            }
        } else if checklist && super::contains(self.hits.overlay_cancel, point) {
            self.leave_work_item_checklist(outcome);
        } else {
            self.overlay = None;
        }
    }

    fn confirm_work_item_choice(&mut self, index: usize, outcome: &mut ClientShellInput) {
        let Some(ClientShellOverlay::WorkItem(overlay)) = self.overlay.as_mut() else {
            return;
        };
        let Some(choice) = overlay.item.choices.get(index).cloned() else {
            return;
        };
        if choice.disabled_reason.is_some() {
            return;
        }
        let method = Method::WorkItemChoose(WorkItemChooseParams {
            item_id: overlay.item.item_id.clone(),
            choice_id: choice.choice_id,
        });
        match choice.action {
            WorkItemChoiceAction::OpenUrl { url } => {
                self.overlay = None;
                outcome.actions.push(ClientShellAction::OpenSafeWebUrl(url));
                self.push_endpoint_method(method, outcome);
            }
            WorkItemChoiceAction::ProvisionWorkspace => {
                overlay.highlighted = index;
                overlay.awaiting_provisioning = true;
                overlay.show_checklist = true;
                // Progress from an earlier attempt must not stand in for this one.
                overlay.item.provisioning = None;
                self.push_endpoint_method(method, outcome);
            }
            WorkItemChoiceAction::Unknown => {}
        }
    }

    fn open_work_item_workspace(&mut self, outcome: &mut ClientShellInput) {
        let Some(ClientShellOverlay::WorkItem(overlay)) = self.overlay.as_ref() else {
            return;
        };
        let Some(workspace_id) = overlay.item.workspace_id.clone() else {
            return;
        };
        self.overlay = None;
        self.push_endpoint_method(
            Method::WorkspaceFocus(WorkspaceTarget { workspace_id }),
            outcome,
        );
    }

    fn leave_work_item_checklist(&mut self, outcome: &mut ClientShellInput) {
        let Some(ClientShellOverlay::WorkItem(overlay)) = self.overlay.take() else {
            return;
        };
        let Some(return_workspace_id) = overlay.return_workspace_id else {
            return;
        };
        let focused = self
            .snapshot
            .as_deref()
            .and_then(|snapshot| snapshot.focused_workspace_id.as_deref());
        if focused != Some(return_workspace_id.as_str())
            && self.workspace_in_snapshot(&return_workspace_id)
        {
            self.push_endpoint_method(
                Method::WorkspaceFocus(WorkspaceTarget {
                    workspace_id: return_workspace_id,
                }),
                outcome,
            );
        }
    }
}

/// Moves the highlight to the previous or next enabled choice, if any.
fn move_highlight(overlay: &mut ClientWorkItemOverlay, step: isize) {
    let len = overlay.item.choices.len();
    if len == 0 {
        return;
    }
    let mut index = overlay.highlighted;
    for _ in 0..len {
        index = (index as isize + step).rem_euclid(len as isize) as usize;
        if overlay.item.choices[index].disabled_reason.is_none() {
            overlay.highlighted = index;
            return;
        }
    }
}
