//! Client-side work items: the per-endpoint projection copy, the sidebar
//! inbox section, and the work-item dialog's state and input.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use super::*;
use crate::api::schema::{
    Method, WorkItemChoiceAction, WorkItemChooseParams, WorkItemHideParams, WorkItemInfo,
    WorkItemLinkParams, WorkItemPhase, WorkItemStepStatus, WorkItemTarget, WorkspaceTarget,
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
    /// The inbox may take most of the sidebar instead of half of it.
    pub(super) expanded: bool,
    /// Visible items skipped at the top of the inbox.
    pub(super) scroll: usize,
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

/// Inbox regions of the last frame, for mouse input.
#[derive(Default)]
pub(super) struct InboxHits {
    pub(super) area: Rect,
    pub(super) header: Rect,
    pub(super) more_above: Rect,
    pub(super) more_below: Rect,
    /// The collapsed sidebar's one-row inbox badge; opens the inbox list.
    pub(super) badge: Rect,
    /// Items drawn in the last frame.
    pub(super) shown: usize,
    /// Items not hidden by dismissing or snoozing.
    pub(super) visible: usize,
}

/// One-row inbox summary for the collapsed sidebar: `●N` with new items, else `·N`.
/// Returns whether it drew, so the caller can give the row up otherwise.
pub(super) fn render_inbox_badge(
    buffer: &mut Buffer,
    rect: Rect,
    projection: &EndpointWorkItemsProjection,
    palette: &Palette,
    hits: &mut ShellHitMap,
) -> bool {
    if rect.is_empty() {
        return false;
    }
    let visible = projection.items.iter().filter(|item| !is_hidden(item));
    let (count, unseen) = visible.fold((0, 0), |(count, unseen), item| {
        (count + 1, unseen + usize::from(!item.seen))
    });
    let failing = projection
        .sources
        .iter()
        .any(|source| source.error.is_some());
    let (text, style) = if unseen > 0 {
        (
            format!("●{unseen}"),
            Style::default()
                .fg(palette.teal)
                .add_modifier(Modifier::BOLD),
        )
    } else if failing {
        ("!".to_string(), Style::default().fg(palette.peach))
    } else {
        (format!("·{count}"), Style::default().fg(palette.overlay0))
    };
    put_text(buffer, rect.x, rect.y, rect.width, &text, style);
    hits.inbox.badge = rect;
    true
}

/// Dismissed and snoozed items stay out of the sidebar.
pub(super) fn is_hidden(item: &WorkItemInfo) -> bool {
    item.dismissed || item.snoozed_until.is_some()
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
    /// The choice waiting for a second confirm, when it cannot be undone.
    pub(super) confirming: Option<usize>,
    pub(super) spinner_frame: usize,
}

impl ClientWorkItemOverlay {
    pub(super) fn checklist(&self) -> bool {
        self.show_checklist
    }
}

/// The projection of the active endpoint belonging to its current snapshot, when a source is
/// configured there. The inbox follows the active machine, so requests made from it go to
/// the machine that owns the items.
pub(super) fn active_projection<'a>(
    items: &'a ClientWorkItems,
    endpoint_id: &ClientEndpointId,
    snapshot: &ClientShellSnapshot,
) -> Option<&'a EndpointWorkItemsProjection> {
    items.by_endpoint.get(endpoint_id).filter(|projection| {
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
/// workspace whose item is hidden or scrolled away stays reachable.
pub(super) fn render_items_section<'a>(
    buffer: &mut Buffer,
    area: Rect,
    projection: &'a EndpointWorkItemsProjection,
    snapshot: &ClientShellSnapshot,
    config: &ClientShellConfig,
    view: &ClientWorkItems,
    endpoint_id: &ClientEndpointId,
    hits: &mut ShellHitMap,
) -> (u16, Vec<&'a str>) {
    let mut nested_workspace_ids = Vec::new();
    if area.is_empty() {
        return (0, nested_workspace_ids);
    }
    let palette = &config.palette;
    let visible: Vec<&WorkItemInfo> = projection
        .items
        .iter()
        .filter(|item| !is_hidden(item))
        .collect();
    let hidden = projection.items.len() - visible.len();
    // Expanded, the spaces list keeps its header and one row.
    let budget = if view.expanded {
        area.height.saturating_sub(3)
    } else {
        area.height / 2
    };
    let limit = area.y.saturating_add(3.max(budget).min(area.height));
    let width = area.width.saturating_sub(1);
    let mut y = area.y;
    hits.inbox = InboxHits {
        header: Rect::new(area.x, y, width, 1),
        visible: visible.len(),
        ..InboxHits::default()
    };
    put_text(
        buffer,
        area.x,
        y,
        area.width,
        if view.expanded {
            " inbox ▴"
        } else {
            " inbox"
        },
        Style::default()
            .fg(palette.overlay0)
            .add_modifier(Modifier::BOLD),
    );
    let unseen = visible.iter().filter(|item| !item.seen).count();
    let (status, color) = if unseen > 0 {
        (format!("{unseen} new "), palette.teal)
    } else if hidden > 0 {
        (format!("{hidden} hidden "), palette.overlay0)
    } else if visible.is_empty() {
        ("none ".to_string(), palette.overlay0)
    } else {
        (String::new(), palette.overlay0)
    };
    if !status.is_empty() {
        put_right_text(
            buffer,
            Rect::new(area.x, y, width, 1),
            y,
            &status,
            Style::default().fg(color),
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
            width,
            &format!(" ! {}: {error}", source.label),
            Style::default().fg(palette.peach),
        );
        y += 1;
    }

    let scroll = view.scroll.min(visible.len().saturating_sub(1));
    if scroll > 0 && y < limit {
        hits.inbox.more_above = Rect::new(area.x, y, width, 1);
        put_text(
            buffer,
            area.x,
            y,
            width,
            &format!(" ↑ {scroll} more"),
            Style::default().fg(palette.overlay0),
        );
        y += 1;
    }
    let focused_workspace_id = snapshot.focused_workspace_id.as_deref();
    for (index, item) in visible.iter().copied().enumerate().skip(scroll) {
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
        let remaining = visible.len() - index;
        // Keep one row for the "more" marker unless this is the last item.
        let reserve = u16::from(remaining > 1);
        if y.saturating_add(2 + nested_height + reserve) > limit {
            let row_y = y.min(limit.saturating_sub(1));
            hits.inbox.more_below = Rect::new(area.x, row_y, width, 1);
            put_text(
                buffer,
                area.x,
                row_y,
                width,
                &format!(" ↓ {remaining} more"),
                Style::default().fg(palette.overlay0),
            );
            y = y.saturating_add(1).min(limit);
            break;
        }
        hits.inbox.shown += 1;
        let item_rect = Rect::new(area.x, y, width, 2);
        render_item_rows(
            buffer,
            item_rect,
            item,
            focused_workspace_id,
            view.spinner_frame,
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
                endpoint_id: endpoint_id.clone(),
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
    let used = (y + 1).min(area.bottom()) - area.y;
    hits.inbox.area = Rect::new(area.x, area.y, width, used);
    (used, nested_workspace_ids)
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

/// Keyboard access to every item: `inbox` keybinding (default prefix+i).
#[derive(Debug, Default)]
pub(super) struct ClientInboxOverlay {
    /// Visible items first, then dismissed and snoozed ones.
    pub(super) items: Vec<WorkItemInfo>,
    pub(super) highlighted: usize,
}

fn inbox_order(projection: &EndpointWorkItemsProjection) -> Vec<WorkItemInfo> {
    let (mut items, hidden): (Vec<WorkItemInfo>, Vec<WorkItemInfo>) = projection
        .items
        .iter()
        .cloned()
        .partition(|item| !is_hidden(item));
    items.extend(hidden);
    items
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
        if *endpoint_id == self.active_endpoint_id {
            self.refresh_work_item_overlay();
            self.refresh_inbox_overlay();
        }
        true
    }

    fn local_items_in_inbox_order(&self) -> Option<Vec<WorkItemInfo>> {
        let snapshot = self.snapshot.as_deref()?;
        active_projection(&self.work_items, &self.active_endpoint_id, snapshot).map(inbox_order)
    }

    pub(super) fn open_inbox_overlay(&mut self) {
        match self.local_items_in_inbox_order() {
            Some(items) => {
                self.overlay = Some(ClientShellOverlay::Inbox(ClientInboxOverlay {
                    items,
                    highlighted: 0,
                }));
            }
            None => self.set_endpoint_error("No work item sources are configured."),
        }
    }

    fn refresh_inbox_overlay(&mut self) {
        if !matches!(self.overlay, Some(ClientShellOverlay::Inbox(_))) {
            return;
        }
        let items = self.local_items_in_inbox_order().unwrap_or_default();
        if let Some(ClientShellOverlay::Inbox(inbox)) = self.overlay.as_mut() {
            // Follow the highlighted item when the order changes.
            let current = inbox
                .items
                .get(inbox.highlighted)
                .map(|item| item.item_id.clone());
            inbox.highlighted = current
                .and_then(|id| items.iter().position(|item| item.item_id == id))
                .unwrap_or(inbox.highlighted)
                .min(items.len().saturating_sub(1));
            inbox.items = items;
        }
    }

    /// Keys while the inbox list is open. Returns false for other overlays.
    pub(super) fn route_inbox_overlay_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let Some(ClientShellOverlay::Inbox(inbox)) = self.overlay.as_mut() else {
            return false;
        };
        outcome.repaint = true;
        let last = inbox.items.len().saturating_sub(1);
        let selected = inbox.items.get(inbox.highlighted).cloned();
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                inbox.highlighted = inbox.highlighted.saturating_sub(1)
            }
            KeyCode::Down | KeyCode::Char('j') => {
                inbox.highlighted = inbox.highlighted.saturating_add(1).min(last)
            }
            KeyCode::Esc => self.overlay = None,
            _ => {
                if let Some(item) = selected {
                    self.inbox_item_key(key.code, item, outcome);
                }
            }
        }
        true
    }

    fn inbox_item_key(
        &mut self,
        code: KeyCode,
        item: WorkItemInfo,
        outcome: &mut ClientShellInput,
    ) {
        let hide = |snooze_seconds| {
            Method::WorkItemHide(WorkItemHideParams {
                item_id: item.item_id.clone(),
                snooze_seconds,
            })
        };
        match code {
            KeyCode::Enter => {
                self.overlay = None;
                self.activate_work_item(&item.item_id, outcome);
            }
            KeyCode::Right | KeyCode::Char('m') => {
                let (x, y) = self
                    .hits
                    .overlay_choice_rows
                    .iter()
                    .find(|(_, index)| {
                        matches!(
                            &self.overlay,
                            Some(ClientShellOverlay::Inbox(inbox)) if inbox.highlighted == *index
                        )
                    })
                    .map_or((0, 0), |(rect, _)| (rect.x + 2, rect.y));
                self.open_work_item_context_menu(&item.item_id, x, y);
            }
            KeyCode::Char('d') if !is_hidden(&item) => {
                self.push_endpoint_method(hide(None), outcome)
            }
            KeyCode::Char('s') if !is_hidden(&item) => {
                self.push_endpoint_method(hide(Some(60 * 60)), outcome)
            }
            KeyCode::Char('u') if is_hidden(&item) => self.push_endpoint_method(
                Method::WorkItemUnhide(WorkItemTarget {
                    item_id: item.item_id.clone(),
                }),
                outcome,
            ),
            _ => {}
        }
    }

    pub(super) fn handle_inbox_overlay_click(
        &mut self,
        point: (u16, u16),
        right: bool,
        outcome: &mut ClientShellInput,
    ) {
        outcome.repaint = true;
        let Some(index) = self
            .hits
            .overlay_choice_rows
            .iter()
            .find(|(rect, _)| super::contains(*rect, point))
            .map(|(_, index)| *index)
        else {
            if !super::contains(self.hits.overlay_area, point) {
                self.overlay = None;
            }
            return;
        };
        let Some(ClientShellOverlay::Inbox(inbox)) = self.overlay.as_mut() else {
            return;
        };
        inbox.highlighted = index;
        let Some(item_id) = inbox.items.get(index).map(|item| item.item_id.clone()) else {
            return;
        };
        if right {
            self.open_work_item_context_menu(&item_id, point.0, point.1);
        } else {
            self.overlay = None;
            self.activate_work_item(&item_id, outcome);
        }
    }

    fn local_work_item(&self, item_id: &str) -> Option<&WorkItemInfo> {
        let snapshot = self.snapshot.as_deref()?;
        active_projection(&self.work_items, &self.active_endpoint_id, snapshot)?
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
            .and_then(|snapshot| {
                active_projection(&self.work_items, &self.active_endpoint_id, snapshot)
            })
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
                confirming: None,
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

    fn work_item_hit_at(&self, point: (u16, u16)) -> Option<String> {
        self.hits
            .work_items
            .iter()
            .find(|hit| super::contains(hit.rect, point))
            .map(|hit| hit.item_id.clone())
    }

    pub(super) fn handle_work_item_click(
        &mut self,
        point: (u16, u16),
        outcome: &mut ClientShellInput,
    ) -> bool {
        if self.handle_inbox_control_click(point, outcome) {
            return true;
        }
        match self.work_item_hit_at(point) {
            Some(item_id) => self.activate_work_item(&item_id, outcome),
            None => false,
        }
    }

    /// Primary action of an item: its running progress, its workspace, or the choices.
    pub(super) fn activate_work_item(
        &mut self,
        item_id: &str,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let Some(item) = self.local_work_item(item_id).cloned() else {
            return false;
        };
        outcome.repaint = true;
        self.mark_work_item_seen(&item, outcome);
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

    fn mark_work_item_seen(&mut self, item: &WorkItemInfo, outcome: &mut ClientShellInput) {
        if !item.seen {
            self.push_endpoint_method(
                Method::WorkItemMarkSeen(WorkItemTarget {
                    item_id: item.item_id.clone(),
                }),
                outcome,
            );
        }
    }

    /// The inbox header toggles the expanded layout; the "more" rows page through items.
    fn handle_inbox_control_click(
        &mut self,
        point: (u16, u16),
        outcome: &mut ClientShellInput,
    ) -> bool {
        if super::contains(self.hits.inbox.badge, point) {
            self.open_inbox_overlay();
            outcome.repaint = true;
            return true;
        }
        let inbox = &self.hits.inbox;
        let page = inbox.shown.max(1);
        let last = inbox.visible.saturating_sub(1);
        let view = &mut self.work_items;
        if super::contains(inbox.header, point) {
            view.expanded = !view.expanded;
            view.scroll = 0;
        } else if super::contains(inbox.more_below, point) {
            if view.expanded {
                view.scroll = view.scroll.saturating_add(page).min(last);
            } else {
                view.expanded = true;
            }
        } else if super::contains(inbox.more_above, point) {
            view.scroll = view.scroll.saturating_sub(page);
        } else {
            return false;
        }
        outcome.repaint = true;
        true
    }

    /// Wheel over the inbox scrolls it by one item. Returns whether the inbox took it.
    pub(super) fn scroll_inbox(
        &mut self,
        point: (u16, u16),
        delta: isize,
        outcome: &mut ClientShellInput,
    ) -> bool {
        if !super::contains(self.hits.inbox.area, point) {
            return false;
        }
        let last = self.hits.inbox.visible.saturating_sub(1);
        let next = self
            .work_items
            .scroll
            .min(last)
            .saturating_add_signed(delta)
            .min(last);
        if next != self.work_items.scroll {
            self.work_items.scroll = next;
            outcome.repaint = true;
        }
        true
    }

    /// Right-click on an item row. Returns false when `point` is not on an item.
    pub(super) fn open_work_item_context_menu_at(&mut self, point: (u16, u16)) -> bool {
        match self.work_item_hit_at(point) {
            Some(item_id) => self.open_work_item_context_menu(&item_id, point.0, point.1),
            None => false,
        }
    }

    pub(super) fn open_work_item_context_menu(&mut self, item_id: &str, x: u16, y: u16) -> bool {
        let Some(item) = self.local_work_item(item_id) else {
            return false;
        };
        let workspace = item.workspace_id.as_deref().and_then(|workspace_id| {
            self.snapshot.as_deref().and_then(|snapshot| {
                snapshot
                    .workspaces
                    .iter()
                    .find(|workspace| workspace.workspace_id == workspace_id)
            })
        });
        let target = ClientContextMenuTarget::WorkItem {
            item_id: item.item_id.clone(),
            workspace_id: workspace.map(|workspace| workspace.workspace_id.clone()),
            is_linked_worktree: workspace
                .and_then(|workspace| workspace.worktree.as_ref())
                .is_some_and(|worktree| worktree.is_linked_worktree),
            has_progress: item.provisioning.is_some(),
            hidden: is_hidden(item),
            link_target: self
                .snapshot
                .as_deref()
                .and_then(|snapshot| snapshot.focused_workspace_id.clone())
                .filter(|focused| item.workspace_id.as_deref() != Some(focused.as_str())),
        };
        self.overlay = Some(ClientShellOverlay::ContextMenu(ClientContextMenuOverlay {
            target,
            x,
            y,
            highlighted: 0,
        }));
        true
    }

    pub(super) fn activate_work_item_context_action(
        &mut self,
        item_id: String,
        workspace_id: Option<String>,
        link_target: Option<String>,
        action: ClientContextMenuAction,
        outcome: &mut ClientShellInput,
    ) {
        const HOUR: u64 = 60 * 60;
        let Some(item) = self.local_work_item(&item_id).cloned() else {
            return;
        };
        let hide = |snooze_seconds| {
            Method::WorkItemHide(WorkItemHideParams {
                item_id: item_id.clone(),
                snooze_seconds,
            })
        };
        match action {
            ClientContextMenuAction::WorkItemChoose => {
                self.mark_work_item_seen(&item, outcome);
                self.open_work_item_overlay(item, false);
            }
            ClientContextMenuAction::WorkItemProgress => self.open_work_item_overlay(item, true),
            ClientContextMenuAction::WorkItemFocus => {
                if let Some(workspace_id) = workspace_id {
                    self.mark_work_item_seen(&item, outcome);
                    self.push_endpoint_method(
                        Method::WorkspaceFocus(WorkspaceTarget { workspace_id }),
                        outcome,
                    );
                }
            }
            ClientContextMenuAction::WorkItemOpenUrl => {
                self.mark_work_item_seen(&item, outcome);
                self.open_web_link(item.url, outcome);
            }
            ClientContextMenuAction::WorkItemSnoozeHour => {
                self.push_endpoint_method(hide(Some(HOUR)), outcome)
            }
            ClientContextMenuAction::WorkItemSnoozeDay => {
                self.push_endpoint_method(hide(Some(24 * HOUR)), outcome)
            }
            ClientContextMenuAction::WorkItemDismiss => {
                self.push_endpoint_method(hide(None), outcome)
            }
            ClientContextMenuAction::WorkItemLink => {
                if let Some(workspace_id) = link_target {
                    self.push_endpoint_method(
                        Method::WorkItemLink(WorkItemLinkParams {
                            item_id: item_id.clone(),
                            workspace_id,
                        }),
                        outcome,
                    );
                }
            }
            ClientContextMenuAction::WorkItemUnhide => self
                .push_endpoint_method(Method::WorkItemUnhide(WorkItemTarget { item_id }), outcome),
            ClientContextMenuAction::RemoveWorktree | ClientContextMenuAction::Close => {
                if let Some(workspace_id) = workspace_id {
                    self.activate_workspace_context_action(workspace_id, action, outcome);
                }
            }
            _ => {}
        }
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
        // Choices that cannot be undone ask first; the second confirm carries them out.
        if choice.confirm.is_some() && overlay.confirming != Some(index) {
            overlay.highlighted = index;
            overlay.confirming = Some(index);
            return;
        }
        let method = Method::WorkItemChoose(WorkItemChooseParams {
            item_id: overlay.item.item_id.clone(),
            choice_id: choice.choice_id,
        });
        match choice.action {
            WorkItemChoiceAction::OpenUrl { url } => {
                self.overlay = None;
                self.open_web_link(url, outcome);
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
            // The server carries it out; the item's spinner shows it running.
            WorkItemChoiceAction::Perform => {
                self.overlay = None;
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
    overlay.confirming = None;
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
