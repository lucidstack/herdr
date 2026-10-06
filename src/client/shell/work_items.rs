//! Client-side work items: the per-endpoint projection copy, the sidebar
//! inbox section, and the work-item dialog's state and input.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use super::*;
use crate::api::schema::{
    Method, WorkItemChoiceAction, WorkItemChoiceInfo, WorkItemChoiceOptionInfo,
    WorkItemChooseParams, WorkItemHideParams, WorkItemInfo, WorkItemLinkParams, WorkItemPhase,
    WorkItemRepositoryInfo, WorkItemStepStatus, WorkItemTarget, WorkspaceTarget,
    WORK_ITEM_PULL_REQUEST_CHOICE_PREFIX,
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
    /// The inbox just emptied: its header sparkles until then.
    pub(super) celebrate_until: Option<Instant>,
}

/// How long the header sparkles after the last ticket leaves the inbox.
const CELEBRATION: Duration = Duration::from_secs(4);
/// Header frames while celebrating, alternating on the spinner clock.
const CELEBRATION_FRAMES: [&str; 2] = ["✦ inbox zero ✦ ", "✧ inbox zero ✧ "];

fn listed_count(projection: &EndpointWorkItemsProjection) -> usize {
    projection
        .items
        .iter()
        .filter(|item| is_listed(item))
        .count()
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
    /// The "+ Pick next task…" row; opens the "Pick next" dialog.
    pub(super) pick_next: Rect,
    /// Repository rows, with their index in the projection's `repositories`.
    pub(super) repositories: Vec<(Rect, usize)>,
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
    let visible = projection.items.iter().filter(|item| is_listed(item));
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

/// Items the inbox lists as tickets: not hidden, not a provider's "Pick next" discovery
/// row, which has its own place at the bottom of the inbox, and not a pull request shown
/// with the item it was opened for.
pub(super) fn is_listed(item: &WorkItemInfo) -> bool {
    !is_hidden(item) && !item.is_pick_next && item.folded_into.is_none()
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
    /// Options the user switched away from their defaults, as (choice id, option id) pairs.
    /// Kept apart from `item`, which a newer projection replaces while the dialog is open.
    pub(super) flipped: HashSet<(String, String)>,
}

impl ClientWorkItemOverlay {
    pub(super) fn checklist(&self) -> bool {
        self.show_checklist
    }

    fn is_flipped(&self, choice: &WorkItemChoiceInfo, option: &WorkItemChoiceOptionInfo) -> bool {
        self.flipped
            .contains(&(choice.choice_id.clone(), option.option_id.clone()))
    }

    /// Whether `option` of `choice` is switched on as things stand.
    pub(super) fn option_on(
        &self,
        choice: &WorkItemChoiceInfo,
        option: &WorkItemChoiceOptionInfo,
    ) -> bool {
        option.default != self.is_flipped(choice, option)
    }

    /// The `options` to send when running `choice`: the ones switched on, or `None` while
    /// every switch is at its default, which the server applies by itself.
    pub(super) fn chosen_options(&self, choice: &WorkItemChoiceInfo) -> Option<Vec<String>> {
        choice
            .options
            .iter()
            .any(|option| self.is_flipped(choice, option))
            .then(|| {
                choice
                    .options
                    .iter()
                    .filter(|option| self.option_on(choice, option))
                    .map(|option| option.option_id.clone())
                    .collect()
            })
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

/// Workspaces the inbox shows, top to bottom whatever its scroll, as snapshot indices: each
/// listed ticket's, each repository's home not already under a ticket, then open "Pick next"
/// workspaces. Keyboard workspace cycling follows this order.
pub(super) fn inbox_workspace_order(
    projection: &EndpointWorkItemsProjection,
    snapshot: &ClientShellSnapshot,
) -> Vec<usize> {
    let index_of = |workspace_id: &str| {
        snapshot
            .workspaces
            .iter()
            .position(|workspace| workspace.workspace_id == workspace_id)
    };
    let item_workspace_ids: Vec<&str> = projection
        .items
        .iter()
        .filter(|item| is_listed(item))
        .filter_map(|item| item.workspace_id.as_deref())
        .collect();
    let mut order: Vec<usize> = Vec::new();
    let tickets = item_workspace_ids.iter().filter_map(|id| index_of(id));
    let homes = repository_homes(projection, snapshot, &item_workspace_ids)
        .into_iter()
        .filter_map(|(_, home)| home.map(|(index, _)| index));
    let discovery = projection
        .items
        .iter()
        .filter(|item| item.is_pick_next)
        .filter_map(|item| item.workspace_id.as_deref().and_then(index_of));
    for index in tickets.chain(homes).chain(discovery) {
        if !order.contains(&index) {
            order.push(index);
        }
    }
    order
}

fn provisioning_running(item: &WorkItemInfo) -> bool {
    item.provisioning
        .as_ref()
        .is_some_and(|provisioning| !provisioning.finished)
}

/// What an item's context menu needs once it is chosen from.
pub(super) struct WorkItemContextMenu {
    pub(super) item_id: String,
    pub(super) workspace_id: Option<String>,
    pub(super) link_target: Option<String>,
    pub(super) groups: Vec<WorkItemMenuGroup>,
    pub(super) links: Vec<WorkItemMenuLink>,
}

/// Choices an item's menu lists before pointing to the dialog for the rest.
const MENU_CHOICES: usize = 2;

/// The item's key as people write it: "TECH-7", or "#12" for a GitHub issue or pull
/// request.
fn display_key(item: &WorkItemInfo) -> &str {
    let reference = item.item_id.rsplit(':').next().unwrap_or(&item.item_id);
    reference
        .rfind('#')
        .map_or(reference, |hash| &reference[hash..])
}

/// The next steps an item's menu offers: its default choice first, then the next ones,
/// up to `MENU_CHOICES`. Opening in the browser has its own rows, and so has the
/// workspace an item already has, which a left click opens. Choices in `left_out` belong
/// to another group of the menu and are not listed again.
fn menu_group(
    item: &WorkItemInfo,
    header: Option<String>,
    more: [&'static str; 2],
    left_out: &HashSet<&str>,
) -> Option<WorkItemMenuGroup> {
    let has_workspace = item.workspace_id.is_some();
    let offered = |choice: &&crate::api::schema::WorkItemChoiceInfo| {
        choice.disabled_reason.is_none()
            && !matches!(choice.action, WorkItemChoiceAction::OpenUrl { .. })
            && !(has_workspace && choice.action == WorkItemChoiceAction::ProvisionWorkspace)
    };
    let listed: Vec<_> = item
        .choices
        .iter()
        .filter(|choice| !left_out.contains(choice.choice_id.as_str()))
        .collect();
    let mut ordered: Vec<_> = listed.iter().copied().filter(offered).collect();
    if let Some(default) = ordered
        .iter()
        .position(|choice| Some(choice.choice_id.as_str()) == item.default_choice_id.as_deref())
    {
        let default = ordered.remove(default);
        ordered.insert(0, default);
    }
    let choices: Vec<(String, String)> = ordered
        .iter()
        .take(MENU_CHOICES)
        .map(|choice| (choice.choice_id.clone(), choice.label.clone()))
        .collect();
    let unlisted = listed.iter().any(|choice| {
        !matches!(choice.action, WorkItemChoiceAction::OpenUrl { .. })
            && !choices.iter().any(|(id, _)| *id == choice.choice_id)
    });
    let [more_choices, all_choices] = more;
    let more_label = unlisted.then_some(if choices.is_empty() {
        all_choices
    } else {
        more_choices
    });
    (!choices.is_empty() || more_label.is_some()).then_some(WorkItemMenuGroup {
        item_id: item.item_id.clone(),
        header,
        choices,
        more_label,
    })
}

/// The item's next steps, then those of the pull request folded into it, each under a
/// header when both are there. The pull request's group leaves out the choices the item
/// carries for it.
fn menu_groups(item: &WorkItemInfo, pull_request: Option<&WorkItemInfo>) -> Vec<WorkItemMenuGroup> {
    let own_header = pull_request.map(|_| match item.tracker_state.as_deref() {
        Some(state) => format!(
            "{} · {}",
            display_key(item),
            state.split(" · ").next().unwrap_or(state)
        ),
        None => display_key(item).to_string(),
    });
    let own = menu_group(
        item,
        own_header,
        ["More choices...", "Choose what to do..."],
        &HashSet::new(),
    );
    // The item offers some of its pull request's choices itself, under prefixed ids. Matching
    // those ids rather than labels keeps two different choices alike in name apart.
    let carried: HashSet<&str> = item
        .choices
        .iter()
        .filter_map(|choice| {
            choice
                .choice_id
                .strip_prefix(WORK_ITEM_PULL_REQUEST_CHOICE_PREFIX)
        })
        .collect();
    // The pull request's own line, e.g. "#11938 ready to merge".
    let pull_request = pull_request.and_then(|pull_request| {
        let header = pull_request
            .context
            .split(" · ")
            .next()
            .unwrap_or(&pull_request.context)
            .to_string();
        menu_group(
            pull_request,
            Some(header),
            ["More pull request choices...", "Pull request choices..."],
            &carried,
        )
    });
    let mut groups: Vec<_> = own.into_iter().chain(pull_request).collect();
    // What can be done straight away leads; a group that only points to its dialog follows.
    groups.sort_by_key(|group| group.choices.is_empty());
    // A lone group needs no header: it is the item that was clicked.
    if let [group] = groups.as_mut_slice() {
        if group.item_id == item.item_id {
            group.header = None;
        }
    }
    groups
}

/// Where the item, and its pull request, open in the browser, each named by key and
/// service: "Open TECH-7 in Jira", "Open #12 on GitHub".
fn menu_links(
    item: &WorkItemInfo,
    pull_request: Option<&WorkItemInfo>,
    sources: &[crate::api::schema::WorkItemSourceInfo],
) -> Vec<WorkItemMenuLink> {
    let link = |key: &str, source_id: &str, url: &str| {
        let service = sources
            .iter()
            .find(|source| source.source_id == source_id)
            .map_or(source_id, |source| source.label.as_str());
        let on = if service.eq_ignore_ascii_case("github") {
            "on"
        } else {
            "in"
        };
        WorkItemMenuLink {
            label: format!("Open {key} {on} {service}"),
            url: url.to_string(),
        }
    };
    let mut links = vec![link(display_key(item), &item.source_id, &item.url)];
    let pull_request = match pull_request {
        Some(pull_request) => Some(link(
            display_key(pull_request),
            &pull_request.source_id,
            &pull_request.url,
        )),
        None => item.linked_pull_request.as_ref().map(|pull_request| {
            link(
                &format!("#{}", pull_request.number),
                &pull_request.source_id,
                &pull_request.url,
            )
        }),
    };
    links.extend(pull_request.filter(|pull_request| pull_request.url != item.url));
    links
}

/// Whether provisioning failed and the failure is what the item needs you for. One that has
/// settled since, such as a brief that looked undelivered until its agent worked, is history:
/// the row then shows the agent's status like that of any item that needs nothing.
fn provisioning_failed(item: &WorkItemInfo) -> bool {
    item.attention
        .as_ref()
        .is_some_and(|attention| attention.kind == crate::api::schema::AttentionKind::Failed)
        && item.provisioning.as_ref().is_some_and(|provisioning| {
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
    snapshot: &'a ClientShellSnapshot,
    config: &ClientShellConfig,
    view: &ClientWorkItems,
    endpoint_id: &ClientEndpointId,
    hits: &mut ShellHitMap,
    has_next_section: bool,
) -> (u16, Vec<&'a str>) {
    let mut nested_workspace_ids = Vec::new();
    if area.is_empty() {
        return (0, nested_workspace_ids);
    }
    let palette = &config.palette;
    let visible: Vec<&WorkItemInfo> = projection
        .items
        .iter()
        .filter(|item| is_listed(item))
        .collect();
    // Hosts of a folded pull request item, found once per frame rather than per row.
    let folding_hosts: HashSet<&str> = projection
        .items
        .iter()
        .filter_map(|item| item.folded_into.as_deref())
        .collect();
    let hidden = projection
        .items
        .iter()
        .filter(|item| !item.is_pick_next && item.folded_into.is_none() && is_hidden(item))
        .count();
    // With a following section (normally spaces), keep this to a share of the
    // zone instead of consuming all of it; expanded keeps that follower to a
    // header and one row. Alone, or last, there is nothing else to save room
    // for, so use the full zone.
    let budget = if !has_next_section {
        area.height
    } else if view.expanded {
        area.height.saturating_sub(3)
    } else {
        area.height / 2
    };
    let limit = area.y.saturating_add(3.max(budget).min(area.height));
    let focused_workspace_id = snapshot.focused_workspace_id.as_deref();
    // "Pick next" discovery rows: one per provider whose workspace is starting or open,
    // expanded only while focused, then the "+ Pick next task…" row. Their workspaces
    // live here and never in the spaces list.
    let discovery: Vec<(&WorkItemInfo, Option<(usize, &ClientShellWorkspace)>)> = projection
        .items
        .iter()
        .filter(|item| item.is_pick_next)
        .filter_map(|item| {
            let workspace = item.workspace_id.as_deref().and_then(|workspace_id| {
                snapshot
                    .workspaces
                    .iter()
                    .enumerate()
                    .find(|(_, workspace)| workspace.workspace_id == workspace_id)
            });
            (workspace.is_some() || provisioning_running(item)).then_some((item, workspace))
        })
        .collect();
    let discovery_rows: u16 = discovery
        .iter()
        .map(|(_, workspace)| match workspace {
            Some((_, workspace)) if workspace.focused => {
                1 + super::render::sidebar::workspace_rows(
                    workspace,
                    workspace.agent_status,
                    true,
                    &config.spaces,
                )
                .len()
                .max(1) as u16
            }
            _ => 1,
        })
        .sum();
    let item_workspace_ids: Vec<&str> = visible
        .iter()
        .filter_map(|item| item.workspace_id.as_deref())
        .collect();
    let repositories = repository_homes(projection, snapshot, &item_workspace_ids);
    let nested_rows = |workspace: &ClientShellWorkspace| {
        super::render::sidebar::workspace_rows(
            workspace,
            workspace.agent_status,
            true,
            &config.spaces,
        )
        .len()
        .max(1) as u16
    };
    let repository_rows: u16 = u16::from(!repositories.is_empty())
        + repositories
            .iter()
            .map(|(_, home)| match home {
                Some((_, workspace)) if workspace.focused => 1 + nested_rows(workspace),
                _ => 1,
            })
            .sum::<u16>();
    // Tickets keep at least one row below the header; the footer gives way first. The
    // blank rows around the "Pick next" rows are not reserved: they take from the
    // following section rather than from the tickets.
    let footer_rows = discovery_rows + repository_rows + 1;
    let items_limit = limit
        .saturating_sub(footer_rows)
        .max(area.y.saturating_add(2).min(limit));
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
    } else if visible.is_empty() && view.celebrate_until.is_some() {
        (
            CELEBRATION_FRAMES[(view.spinner_frame / 2) % CELEBRATION_FRAMES.len()].to_string(),
            palette.green,
        )
    } else if hidden > 0 {
        (format!("{hidden} hidden "), palette.overlay0)
    } else if visible.is_empty() {
        ("all clear ".to_string(), palette.green)
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
        if y >= items_limit {
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
    if scroll > 0 && y < items_limit {
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
    for (index, item) in visible.iter().copied().enumerate().skip(scroll) {
        let workspace = item.workspace_id.as_deref().and_then(|workspace_id| {
            snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.workspace_id == workspace_id)
        });
        let remaining = visible.len() - index;
        let item_height = item_rows(item);
        // Keep one row for the "more" marker unless this is the last item.
        let reserve = u16::from(remaining > 1);
        if y.saturating_add(item_height + reserve) > items_limit {
            let row_y = y.min(items_limit.saturating_sub(1));
            hits.inbox.more_below = Rect::new(area.x, row_y, width, 1);
            put_text(
                buffer,
                area.x,
                row_y,
                width,
                &format!(" ↓ {remaining} more"),
                Style::default().fg(palette.overlay0),
            );
            y = y.saturating_add(1).min(items_limit);
            break;
        }
        hits.inbox.shown += 1;
        let item_rect = Rect::new(area.x, y, width, item_height);
        render_item_rows(
            buffer,
            item_rect,
            item,
            folding_hosts.contains(item.item_id.as_str()),
            workspace.map(|workspace| {
                (
                    status_icon(workspace.agent_status, config.status_indicators),
                    status_color(workspace.agent_status, palette),
                )
            }),
            focused_workspace_id,
            view.spinner_frame,
            config.service_icons,
            palette,
        );
        hits.work_items.push(WorkItemHit {
            rect: item_rect,
            item_id: item.item_id.clone(),
        });
        y += item_height;
        // The item stands for its workspace: a click on it focuses the workspace, and its
        // agent's status shows on the item's first row.
        if let Some(workspace) = workspace {
            nested_workspace_ids.push(workspace.workspace_id.as_str());
        }
    }
    // Blank row setting the "Pick next" rows apart from the tickets.
    y = y.saturating_add(1).min(area.bottom());
    for (item, workspace) in discovery {
        if let Some(workspace_id) = item.workspace_id.as_deref() {
            nested_workspace_ids.push(workspace_id);
        }
        if y >= area.bottom() {
            continue;
        }
        let row = Rect::new(area.x, y, width, 1);
        let focused = workspace.is_some_and(|(_, workspace)| workspace.focused);
        // Starting: the provisioning spinner; open: the agent's status, like a space.
        let glyph = match workspace {
            Some((_, workspace)) => (
                status_icon(workspace.agent_status, config.status_indicators),
                status_color(workspace.agent_status, palette),
            ),
            None => (
                SPINNER_FRAMES[view.spinner_frame % SPINNER_FRAMES.len()],
                palette.yellow,
            ),
        };
        render_footer_row(buffer, row, glyph, &item.title, None, focused, palette);
        hits.work_items.push(WorkItemHit {
            rect: row,
            item_id: item.item_id.clone(),
        });
        y += 1;
        if let Some((workspace_index, workspace)) = workspace.filter(|_| focused) {
            y += render_nested_workspace(
                buffer,
                Rect::new(area.x, y, width, area.bottom().saturating_sub(y)),
                workspace_index,
                workspace,
                config,
                endpoint_id,
                hits,
            );
        }
    }
    if y < area.bottom() {
        hits.inbox.pick_next = Rect::new(area.x, y, width, 1);
        put_text(
            buffer,
            area.x,
            y,
            width,
            " + Pick next task…",
            Style::default().fg(palette.overlay0),
        );
        y += 1;
    }
    // Blank row separating them from the repositories.
    if !repositories.is_empty() {
        y = y.saturating_add(1).min(area.bottom());
    }
    if !repositories.is_empty() && y < area.bottom() {
        put_text(
            buffer,
            area.x,
            y,
            width,
            " repositories",
            Style::default()
                .fg(palette.overlay0)
                .add_modifier(Modifier::BOLD),
        );
        y += 1;
    }
    for (index, (repository, home)) in repositories.into_iter().enumerate() {
        if let Some((_, workspace)) = home {
            nested_workspace_ids.push(workspace.workspace_id.as_str());
        }
        if y >= area.bottom() {
            continue;
        }
        let row = Rect::new(area.x, y, width, 1);
        let focused = home.is_some_and(|(_, workspace)| workspace.focused);
        // Open: its agent status and branch; closed: a dim dot, one click from opening.
        let (glyph, color, branch) = match home {
            Some((_, workspace)) => (
                status_icon(workspace.agent_status, config.status_indicators),
                status_color(workspace.agent_status, palette),
                workspace.branch.as_deref(),
            ),
            None => ("·", palette.overlay0, None),
        };
        render_footer_row(
            buffer,
            row,
            (glyph, color),
            &repository.label,
            branch,
            focused,
            palette,
        );
        hits.inbox.repositories.push((row, index));
        y += 1;
        if let Some((workspace_index, workspace)) = home.filter(|_| focused) {
            y += render_nested_workspace(
                buffer,
                Rect::new(area.x, y, width, area.bottom().saturating_sub(y)),
                workspace_index,
                workspace,
                config,
                endpoint_id,
                hits,
            );
        }
    }
    // Blank separator before the spaces list.
    let used = (y + 1).min(area.bottom()) - area.y;
    hits.inbox.area = Rect::new(area.x, area.y, width, used);
    (used, nested_workspace_ids)
}

/// One inbox row below the tickets: a status glyph, a title and an optional dim detail
/// (e.g. a branch), highlighted while its workspace is focused.
fn render_footer_row(
    buffer: &mut Buffer,
    row: Rect,
    (glyph, color): (&str, ratatui::style::Color),
    title: &str,
    detail: Option<&str>,
    focused: bool,
    palette: &Palette,
) {
    if focused {
        buffer.set_style(row, Style::default().bg(palette.active_row_bg));
    }
    let x = put_segment(buffer, row.x, row.y, row.right(), " ", Style::default());
    let x = put_segment(
        buffer,
        x,
        row.y,
        row.right(),
        glyph,
        Style::default().fg(color),
    );
    let x = put_segment(
        buffer,
        x.saturating_add(1),
        row.y,
        row.right(),
        title,
        Style::default().fg(if focused {
            palette.text
        } else {
            palette.subtext0
        }),
    );
    if let Some(detail) = detail {
        put_segment(
            buffer,
            x.saturating_add(1),
            row.y,
            row.right(),
            detail,
            Style::default().fg(palette.overlay0),
        );
    }
}

/// The focused workspace nested under its inbox row, at the top of `area`. Returns the
/// rows it used.
fn render_nested_workspace(
    buffer: &mut Buffer,
    area: Rect,
    workspace_index: usize,
    workspace: &ClientShellWorkspace,
    config: &ClientShellConfig,
    endpoint_id: &ClientEndpointId,
    hits: &mut ShellHitMap,
) -> u16 {
    let rows = super::render::sidebar::workspace_rows(
        workspace,
        workspace.agent_status,
        true,
        &config.spaces,
    );
    let height = (rows.len().max(1) as u16).min(area.height);
    let rect = Rect::new(area.x, area.y, area.width, height);
    buffer.set_style(rect, Style::default().bg(config.palette.active_row_bg));
    super::render::sidebar::render_workspace_rows(
        buffer,
        rect,
        workspace.agent_status,
        config.status_indicators,
        &WorkspaceEntry {
            index: workspace_index,
            indented: true,
            last_child: true,
        },
        rows,
        true,
        false,
        false,
        false,
        &config.palette,
    );
    hits.workspaces.push(WorkspaceHit {
        rect,
        endpoint_id: endpoint_id.clone(),
        workspace_id: workspace.workspace_id.clone(),
        indented: true,
        group_toggle: None,
    });
    height
}

/// Each mapped repository with its home: the open workspace on its main checkout, unless
/// a ticket already shows that workspace.
fn repository_homes<'a>(
    projection: &'a EndpointWorkItemsProjection,
    snapshot: &'a ClientShellSnapshot,
    item_workspace_ids: &[&str],
) -> Vec<(
    &'a WorkItemRepositoryInfo,
    Option<(usize, &'a ClientShellWorkspace)>,
)> {
    projection
        .repositories
        .iter()
        .map(|repository| {
            let home = repository_home(repository, snapshot).filter(|(_, workspace)| {
                !item_workspace_ids.contains(&workspace.workspace_id.as_str())
            });
            (repository, home)
        })
        .collect()
}

/// The open workspace on `repository`'s main checkout, as the server reports it.
fn repository_home<'a>(
    repository: &WorkItemRepositoryInfo,
    snapshot: &'a ClientShellSnapshot,
) -> Option<(usize, &'a ClientShellWorkspace)> {
    let workspace_id = repository.workspace_id.as_deref()?;
    snapshot
        .workspaces
        .iter()
        .enumerate()
        .find(|(_, workspace)| workspace.workspace_id == workspace_id)
}

/// Context and title, plus one status line per service the item is on: its own tracker's
/// or, for a pull request, its own state; the ticket its title names; then its linked pull
/// request's.
fn item_rows(item: &WorkItemInfo) -> u16 {
    2 + u16::from(item.tracker_state.is_some())
        + u16::from(item.own_pull_request.is_some())
        + u16::from(item.linked_ticket.is_some())
        + u16::from(item.linked_pull_request.is_some())
}

/// The mark in front of a status line from `source_id`.
fn service_mark(icons: crate::config::ServiceIcons, source_id: &str) -> Option<&'static str> {
    use crate::config::ServiceIcons;
    match (icons, source_id) {
        (ServiceIcons::Text, "jira") => Some("jira"),
        (ServiceIcons::Text, "github") => Some("gh"),
        // nf-dev-jira, nf-fa-github.
        (ServiceIcons::Nerd, "jira") => Some("\u{e75c}"),
        (ServiceIcons::Nerd, "github") => Some("\u{f09b}"),
        _ => None,
    }
}

/// The mark in front of a pull request's context while it is part of a stack: its place and
/// the stack's size, e.g. "≡ 2/3 ".
fn stack_mark(stack: &crate::api::schema::WorkItemPullRequestStackInfo) -> String {
    format!("≡ {}/{} ", stack.position, stack.entries.len())
}

#[allow(clippy::too_many_arguments)] // Row inputs; a struct would only rename them.
fn render_item_rows(
    buffer: &mut Buffer,
    rect: Rect,
    item: &WorkItemInfo,
    has_folded: bool,
    agent: Option<(&str, ratatui::style::Color)>,
    focused_workspace_id: Option<&str>,
    spinner_frame: usize,
    icons: crate::config::ServiceIcons,
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
        // The workspace's agent, the way the spaces list shows it.
        agent
    };
    let status_width = u16::from(status.is_some()) * 2;
    let context_style = if item.seen {
        Style::default().fg(palette.mauve)
    } else {
        Style::default()
            .fg(palette.mauve)
            .add_modifier(Modifier::BOLD)
    };
    let mut x = x.saturating_add(1);
    let context_right = rect.right().saturating_sub(status_width);
    if let Some(stack) = item
        .own_pull_request
        .as_ref()
        .and_then(|pull| pull.stack.as_ref())
    {
        x = put_segment(
            buffer,
            x,
            rect.y,
            context_right,
            &stack_mark(stack),
            Style::default().fg(palette.blue),
        );
    }
    put_segment(
        buffer,
        x,
        rect.y,
        context_right,
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
    let mut y = title_y.saturating_add(1);
    let mut status_row = |source_id: &str, text: &str, style: Style| {
        let mut x = rect.x.saturating_add(3);
        if let Some(mark) = service_mark(icons, source_id) {
            x = put_segment(
                buffer,
                x,
                y,
                rect.right(),
                &format!("{mark} "),
                Style::default().fg(palette.overlay0),
            );
        }
        put_segment(buffer, x, y, rect.right(), text, style);
        y = y.saturating_add(1);
    };
    if let Some(state) = &item.tracker_state {
        // Highlighted while the tracker lags behind work you started on the item.
        if item.start_reminder.is_some() {
            status_row(
                &item.source_id,
                &format!(" {state} "),
                Style::default()
                    .fg(panel_contrast_fg(palette))
                    .bg(palette.yellow),
            );
        } else {
            status_row(
                &item.source_id,
                state,
                Style::default().fg(palette.overlay0),
            );
        }
    }
    // The item is the pull request: its number is in the context row already.
    if let Some(pull_request) = &item.own_pull_request {
        status_row(
            &pull_request.source_id,
            &pull_request.status,
            Style::default().fg(palette.overlay0),
        );
    }
    if let Some(ticket) = &item.linked_ticket {
        status_row(
            &ticket.source_id,
            &format!("{} · {}", ticket.key, ticket.tracker_state),
            Style::default().fg(palette.overlay0),
        );
    }
    if let Some(pull_request) = &item.linked_pull_request {
        // Amber while it waits on you: a draft to mark ready, or its own inbox event.
        let color = if pull_request.is_draft || has_folded {
            palette.yellow
        } else {
            palette.overlay0
        };
        status_row(
            &pull_request.source_id,
            &format!("#{} · {}", pull_request.number, pull_request.status),
            Style::default().fg(color),
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
        .filter(|item| !item.is_pick_next && item.folded_into.is_none())
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
        let listed_before = self
            .work_items
            .by_endpoint
            .get(endpoint_id)
            .filter(|current| current.boot_id == projection.boot_id)
            .map(listed_count);
        let listed_after = listed_count(&projection);
        if !self.work_items.store(endpoint_id, projection) {
            return false;
        }
        if *endpoint_id == self.active_endpoint_id {
            // Celebrate the last ticket leaving, not an inbox that starts out empty.
            if listed_after > 0 {
                self.work_items.celebrate_until = None;
            } else if listed_before.is_some_and(|before| before > 0) {
                self.work_items.celebrate_until = Some(Instant::now() + CELEBRATION);
            }
            self.refresh_work_item_overlay();
            self.refresh_inbox_overlay();
        }
        true
    }

    fn local_items_in_inbox_order(&self) -> Option<Vec<WorkItemInfo>> {
        let snapshot = self.snapshot.as_deref()?;
        active_projection(&self.work_items, &self.active_endpoint_id, snapshot).map(inbox_order)
    }

    /// A repository row: its main checkout's open workspace, or a new one there on
    /// whatever is checked out.
    fn open_repository(&mut self, index: usize, outcome: &mut ClientShellInput) {
        let Some(snapshot) = self.snapshot.as_deref() else {
            return;
        };
        let Some(repository) =
            active_projection(&self.work_items, &self.active_endpoint_id, snapshot)
                .and_then(|projection| projection.repositories.get(index))
        else {
            return;
        };
        let method = match repository_home(repository, snapshot) {
            Some((_, workspace)) => Method::WorkspaceFocus(WorkspaceTarget {
                workspace_id: workspace.workspace_id.clone(),
            }),
            None => Method::WorkspaceCreate(crate::api::schema::WorkspaceCreateParams {
                source_workspace_id: None,
                cwd: Some(repository.path.clone()),
                focus: true,
                label: None,
                env: Default::default(),
            }),
        };
        self.push_endpoint_method(method, outcome);
    }

    /// The "Pick next task" dialog: the provider (preselecting the last one used) and
    /// optional context, pre-filled with the last text sent to that provider.
    pub(super) fn open_pick_next_overlay(&mut self) {
        let Some(projection) = self.snapshot.as_deref().and_then(|snapshot| {
            active_projection(&self.work_items, &self.active_endpoint_id, snapshot)
        }) else {
            self.set_endpoint_error("No work item sources are configured.");
            return;
        };
        let sources: Vec<(String, String)> = projection
            .sources
            .iter()
            .map(|source| (source.source_id.clone(), source.label.clone()))
            .collect();
        let selected = projection
            .pick_next
            .last_source_id
            .as_deref()
            .and_then(|last| sources.iter().position(|(source_id, _)| source_id == last))
            .unwrap_or(0);
        let last_context = projection.pick_next.last_context.clone();
        let text = sources
            .get(selected)
            .and_then(|(source_id, _)| last_context.get(source_id))
            .map_or("", String::as_str);
        self.overlay = Some(ClientShellOverlay::Rename(ClientRenameOverlay {
            title: "pick next task",
            input: TextEditor::new(text, true),
            target: ClientRenameTarget::PickNext {
                sources,
                selected,
                last_context,
            },
        }));
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
                overlay.item.running_choice_id.is_some()
                    || (overlay.awaiting_provisioning && overlay.item.provisioning.is_none())
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
        let celebrating = match self.work_items.celebrate_until {
            Some(until) if now >= until => {
                self.work_items.celebrate_until = None;
                // Settle into the calm header straight away.
                return true;
            }
            until => until.is_some(),
        };
        if !sidebar_animates && !overlay_animates && !celebrating {
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
                flipped: HashSet::new(),
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
        if let Some(index) = self
            .hits
            .inbox
            .repositories
            .iter()
            .find(|(rect, _)| super::contains(*rect, point))
            .map(|(_, index)| *index)
        {
            self.open_repository(index, outcome);
            outcome.repaint = true;
            return true;
        }
        if super::contains(self.hits.inbox.pick_next, point) {
            self.open_pick_next_overlay();
            outcome.repaint = true;
            return true;
        }
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
        let snapshot = self.snapshot.as_deref();
        let workspace = item.workspace_id.as_deref().and_then(|workspace_id| {
            snapshot.and_then(|snapshot| {
                snapshot
                    .workspaces
                    .iter()
                    .find(|workspace| workspace.workspace_id == workspace_id)
            })
        });
        let sources = snapshot
            .and_then(|snapshot| {
                active_projection(&self.work_items, &self.active_endpoint_id, snapshot)
            })
            .map(|projection| projection.sources.as_slice())
            .unwrap_or_default();
        let pull_request = self.folded_pull_request_item(item_id);
        let target = ClientContextMenuTarget::WorkItem {
            item_id: item.item_id.clone(),
            workspace_id: workspace.map(|workspace| workspace.workspace_id.clone()),
            is_linked_worktree: workspace
                .and_then(|workspace| workspace.worktree.as_ref())
                .is_some_and(|worktree| worktree.is_linked_worktree),
            has_progress: item.provisioning.is_some(),
            hidden: is_hidden(item),
            link_target: snapshot
                .and_then(|snapshot| snapshot.focused_workspace_id.clone())
                .filter(|focused| item.workspace_id.as_deref() != Some(focused.as_str())),
            groups: menu_groups(item, pull_request),
            links: menu_links(item, pull_request, sources),
        };
        self.overlay = Some(ClientShellOverlay::ContextMenu(
            ClientContextMenuOverlay::new(target, x, y),
        ));
        true
    }

    pub(super) fn activate_work_item_context_action(
        &mut self,
        menu: WorkItemContextMenu,
        action: ClientContextMenuAction,
        outcome: &mut ClientShellInput,
    ) {
        const HOUR: u64 = 60 * 60;
        let WorkItemContextMenu {
            item_id,
            workspace_id,
            link_target,
            groups,
            links,
        } = menu;
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
            ClientContextMenuAction::WorkItemRunChoice { group, index } => {
                let Some(group) = groups.get(usize::from(group)) else {
                    return;
                };
                let Some((choice_id, _)) = group.choices.get(usize::from(index)) else {
                    return;
                };
                let Some(owner) = self.local_work_item(&group.item_id).cloned() else {
                    return;
                };
                let Some(choice_index) = owner
                    .choices
                    .iter()
                    .position(|choice| &choice.choice_id == choice_id)
                else {
                    return;
                };
                // Through the item's dialog, as if chosen there: it asks before what cannot
                // be undone and shows how the choice ends.
                self.mark_work_item_seen(&owner, outcome);
                self.open_work_item_overlay(owner, false);
                self.confirm_work_item_choice(choice_index, outcome);
            }
            ClientContextMenuAction::WorkItemMoreChoices { group } => {
                let owner = groups
                    .get(usize::from(group))
                    .and_then(|group| self.local_work_item(&group.item_id).cloned());
                if let Some(owner) = owner {
                    self.mark_work_item_seen(&owner, outcome);
                    self.open_work_item_overlay(owner, false);
                }
            }
            ClientContextMenuAction::WorkItemProgress => self.open_work_item_overlay(item, true),
            ClientContextMenuAction::WorkItemOpenLink(index) => {
                if let Some(link) = links.into_iter().nth(usize::from(index)) {
                    self.mark_work_item_seen(&item, outcome);
                    self.open_web_link(link.url, outcome);
                }
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
                    // An item's workspace closes on its own, never as a group.
                    self.activate_workspace_context_action(workspace_id, false, action, outcome);
                }
            }
            _ => {}
        }
    }

    /// The pull request item shown with `host_id` instead of on its own.
    fn folded_pull_request_item(&self, host_id: &str) -> Option<&WorkItemInfo> {
        let snapshot = self.snapshot.as_deref()?;
        active_projection(&self.work_items, &self.active_endpoint_id, snapshot)?
            .items
            .iter()
            .find(|item| item.folded_into.as_deref() == Some(host_id))
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
            KeyCode::Char(digit @ '1'..='9') if key.modifiers.is_empty() => {
                flip_option(overlay, usize::from(digit as u8 - b'1'));
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
        let highlighted = overlay.highlighted;
        let hit = |rows: &[(Rect, usize)]| {
            rows.iter()
                .find(|(rect, _)| super::contains(*rect, point))
                .map(|(_, index)| *index)
                .filter(|_| !checklist)
        };
        if let Some(index) = hit(&self.hits.overlay_option_rows) {
            if let Some(ClientShellOverlay::WorkItem(overlay)) = self.overlay.as_mut() {
                flip_option(overlay, index);
            }
        } else if let Some(index) = hit(&self.hits.overlay_choice_rows) {
            self.click_work_item_choice(index, outcome);
        } else if super::contains(self.hits.overlay_primary, point) {
            if checklist {
                self.open_work_item_workspace(outcome);
            } else {
                self.confirm_work_item_choice(highlighted, outcome);
            }
        } else if checklist && super::contains(self.hits.overlay_cancel, point) {
            self.leave_work_item_checklist(outcome);
        } else {
            self.overlay = None;
        }
    }

    /// A click on a choice runs it. A choice with switches that is not highlighted yet is
    /// highlighted instead, so its switches show and can be set before a second click runs it.
    fn click_work_item_choice(&mut self, index: usize, outcome: &mut ClientShellInput) {
        if let Some(ClientShellOverlay::WorkItem(overlay)) = self.overlay.as_mut() {
            let has_switches = overlay.item.choices.get(index).is_some_and(|choice| {
                !choice.options.is_empty() && choice.disabled_reason.is_none()
            });
            if has_switches && overlay.highlighted != index {
                overlay.highlighted = index;
                overlay.confirming = None;
                return;
            }
        }
        self.confirm_work_item_choice(index, outcome);
    }

    fn confirm_work_item_choice(&mut self, index: usize, outcome: &mut ClientShellInput) {
        let Some(ClientShellOverlay::WorkItem(overlay)) = self.overlay.as_mut() else {
            return;
        };
        let Some(choice) = overlay.item.choices.get(index).cloned() else {
            return;
        };
        // One action at a time, and a finished fix is not repeated.
        let done = overlay
            .item
            .action_outcome
            .as_ref()
            .is_some_and(|outcome| outcome.succeeded && outcome.choice_id == choice.choice_id);
        if choice.disabled_reason.is_some() || overlay.item.running_choice_id.is_some() || done {
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
            choice_id: choice.choice_id.clone(),
            options: overlay.chosen_options(&choice),
            // The dialog sets no model or effort: the agent starts as configured.
            model: None,
            effort: None,
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
            // The dialog stays open and shows how it ends: a spinner on the choice, then
            // the outcome above the choices.
            WorkItemChoiceAction::Perform => {
                overlay.highlighted = index;
                overlay.item.running_choice_id = Some(choice.choice_id.clone());
                overlay.item.action_outcome = None;
                self.push_endpoint_method(method, outcome);
            }
            // A brief goes to the agent, which the server focuses.
            WorkItemChoiceAction::BriefAgent => {
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

/// Switches the "Pick next" dialog to the provider `step` places away, pre-filling
/// that provider's last context. Returns whether anything changed.
pub(super) fn cycle_pick_next_provider(rename: &mut ClientRenameOverlay, step: isize) -> bool {
    let ClientRenameTarget::PickNext {
        sources, selected, ..
    } = &rename.target
    else {
        return false;
    };
    if sources.len() < 2 {
        return false;
    }
    let next = (*selected as isize + step).rem_euclid(sources.len() as isize) as usize;
    select_pick_next_provider(rename, next)
}

/// Selects provider `index` in the "Pick next" dialog. Returns whether it changed.
pub(super) fn select_pick_next_provider(rename: &mut ClientRenameOverlay, index: usize) -> bool {
    let ClientRenameTarget::PickNext {
        sources,
        selected,
        last_context,
    } = &mut rename.target
    else {
        return false;
    };
    if index == *selected || index >= sources.len() {
        return false;
    }
    *selected = index;
    let text = last_context
        .get(&sources[index].0)
        .map_or("", String::as_str);
    rename.input = TextEditor::new(text, true);
    true
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

/// Flips option `index` of the highlighted choice between on and off.
fn flip_option(overlay: &mut ClientWorkItemOverlay, index: usize) {
    let Some(choice) = overlay
        .item
        .choices
        .get(overlay.highlighted)
        .filter(|choice| choice.disabled_reason.is_none())
    else {
        return;
    };
    let Some(option) = choice.options.get(index) else {
        return;
    };
    let flipped = (choice.choice_id.clone(), option.option_id.clone());
    if !overlay.flipped.remove(&flipped) {
        overlay.flipped.insert(flipped);
    }
    // What would run has changed, so a second confirm has to start over.
    overlay.confirming = None;
}
