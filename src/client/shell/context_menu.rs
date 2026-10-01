use std::borrow::Cow;

use super::*;

/// Rows of a menu built group by group. Groups go in order of intent: acting on the
/// target, opening it elsewhere, organising it, setting it aside, then removing it.
#[derive(Default)]
struct MenuRows {
    rows: Vec<ClientContextMenuItem>,
    divider_pending: bool,
}

impl MenuRows {
    /// Separates what comes next from the rows before it, once anything follows.
    fn divide(&mut self) {
        self.divider_pending = !self.rows.is_empty();
    }

    fn push(&mut self, label: Cow<'static, str>, action: Option<ClientContextMenuAction>) {
        if std::mem::take(&mut self.divider_pending) {
            self.rows.push(ClientContextMenuItem {
                label: Cow::Borrowed(""),
                action: None,
            });
        }
        self.rows.push(ClientContextMenuItem { label, action });
    }

    fn action(&mut self, label: impl Into<Cow<'static, str>>, action: ClientContextMenuAction) {
        self.push(label.into(), Some(action));
    }

    fn header(&mut self, label: String) {
        self.push(Cow::Owned(label), None);
    }
}

impl ClientContextMenuOverlay {
    /// A menu at (`x`, `y`) with its first choosable row highlighted.
    pub(super) fn new(target: ClientContextMenuTarget, x: u16, y: u16) -> Self {
        let mut menu = Self {
            target,
            x,
            y,
            highlighted: 0,
        };
        menu.highlighted = menu
            .items()
            .iter()
            .position(|item| item.action.is_some())
            .unwrap_or(0);
        menu
    }

    pub(super) fn items(&self) -> Vec<ClientContextMenuItem> {
        use ClientContextMenuAction as Action;

        let mut menu = MenuRows::default();
        match &self.target {
            ClientContextMenuTarget::Workspace { is_git: false, .. } => {
                menu.action("Rename", Action::Rename);
                menu.divide();
                menu.action("Close", Action::Close);
            }
            ClientContextMenuTarget::Workspace {
                is_linked_worktree: false,
                has_worktree_children: false,
                ..
            } => {
                menu.action("New worktree", Action::NewWorktree);
                menu.action("Open worktree...", Action::OpenWorktree);
                menu.divide();
                menu.action("Rename", Action::Rename);
                menu.divide();
                menu.action("Close", Action::Close);
            }
            ClientContextMenuTarget::Workspace {
                is_linked_worktree: true,
                ..
            } => {
                menu.action("Rename", Action::Rename);
                menu.divide();
                menu.action("Close", Action::Close);
                menu.action("Delete worktree checkout...", Action::RemoveWorktree);
            }
            ClientContextMenuTarget::Workspace {
                has_worktree_children: true,
                close_group,
                collapsed,
                ..
            } => {
                menu.action("New worktree", Action::NewWorktree);
                menu.action("Open worktree...", Action::OpenWorktree);
                menu.divide();
                menu.action("Rename", Action::Rename);
                menu.action(
                    if *collapsed { "Expand" } else { "Collapse" },
                    Action::ToggleGroup,
                );
                menu.divide();
                menu.action(
                    if *close_group { "Close group" } else { "Close" },
                    Action::Close,
                );
            }
            ClientContextMenuTarget::Tab { .. } => {
                menu.action("New tab", Action::NewTab);
                menu.action("Rename", Action::Rename);
                menu.divide();
                menu.action("Close", Action::Close);
            }
            ClientContextMenuTarget::Pane {
                source_pane_id,
                has_manual_label,
                right_click_passthrough,
                ..
            } => {
                menu.action("Rename pane", Action::RenamePane);
                if *has_manual_label {
                    menu.action("Clear pane name", Action::ClearPaneName);
                }
                if source_pane_id.is_some() {
                    menu.action("Swap with focused pane", Action::SwapWithFocusedPane);
                }
                menu.action("Split right", Action::SplitRight);
                menu.action("Split down", Action::SplitDown);
                menu.action("Zoom", Action::Zoom);
                menu.action(
                    if *right_click_passthrough {
                        "Use Herdr right-click menu"
                    } else {
                        "Send right-clicks to pane"
                    },
                    Action::ToggleRightClickPassthrough,
                );
                menu.divide();
                menu.action("Close pane", Action::ClosePane);
            }
            // A left click already opens the item's workspace, progress or choices, so
            // the menu leads with what to do next instead.
            ClientContextMenuTarget::WorkItem {
                workspace_id,
                is_linked_worktree,
                has_progress,
                hidden,
                link_target,
                groups,
                links,
                ..
            } => {
                for (group_index, group) in groups.iter().enumerate() {
                    let group_index = group_index as u8;
                    menu.divide();
                    if let Some(header) = &group.header {
                        menu.header(header.clone());
                    }
                    for (index, (_, label)) in group.choices.iter().enumerate() {
                        menu.action(
                            label.clone(),
                            Action::WorkItemRunChoice {
                                group: group_index,
                                index: index as u8,
                            },
                        );
                    }
                    if let Some(more) = group.more_label {
                        menu.action(more, Action::WorkItemMoreChoices { group: group_index });
                    }
                }
                menu.divide();
                if *has_progress {
                    menu.action("Show progress", Action::WorkItemProgress);
                }
                for (index, link) in links.iter().enumerate() {
                    menu.action(link.label.clone(), Action::WorkItemOpenLink(index as u8));
                }
                // Organise it or set it aside.
                menu.divide();
                if link_target.is_some() {
                    menu.action("Link to current workspace", Action::WorkItemLink);
                }
                if *hidden {
                    menu.action("Show in inbox again", Action::WorkItemUnhide);
                } else {
                    menu.action("Snooze for 1 hour", Action::WorkItemSnoozeHour);
                    menu.action("Snooze for 1 day", Action::WorkItemSnoozeDay);
                    menu.action("Dismiss", Action::WorkItemDismiss);
                }
                if workspace_id.is_some() {
                    menu.divide();
                    if *is_linked_worktree {
                        menu.action("Delete worktree checkout...", Action::RemoveWorktree);
                    } else {
                        menu.action("Close workspace", Action::Close);
                    }
                }
            }
        }
        menu.rows
    }
}

impl ClientShellState {
    pub(super) fn open_workspace_context_menu(&mut self, workspace_id: String, x: u16, y: u16) {
        let Some(snapshot) = self.snapshot.as_deref() else {
            return;
        };
        let Some(workspace) = snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.workspace_id == workspace_id)
        else {
            return;
        };
        let worktree = workspace.worktree.as_ref();
        let has_worktree_children = worktree.is_some_and(|worktree| {
            !worktree.is_linked_worktree
                && snapshot.workspaces.iter().any(|candidate| {
                    candidate.worktree.as_ref().is_some_and(|candidate| {
                        candidate.key == worktree.key && candidate.is_linked_worktree
                    })
                })
        });
        let close_group = super::sidebar::workspace_close_is_group(snapshot, workspace);
        let collapsed = worktree.is_some_and(|worktree| {
            self.group_is_collapsed(&self.active_endpoint_id, &worktree.key)
        });
        self.overlay = Some(ClientShellOverlay::ContextMenu(
            ClientContextMenuOverlay::new(
                ClientContextMenuTarget::Workspace {
                    workspace_id,
                    is_git: worktree.is_some() || workspace.branch.is_some(),
                    is_linked_worktree: worktree
                        .is_some_and(|worktree| worktree.is_linked_worktree),
                    has_worktree_children,
                    close_group,
                    collapsed,
                },
                x,
                y,
            ),
        ));
    }

    pub(super) fn open_tab_context_menu(&mut self, tab_id: String, x: u16, y: u16) {
        let Some(tab) = self
            .snapshot
            .as_deref()
            .and_then(|snapshot| snapshot.tabs.iter().find(|tab| tab.tab_id == tab_id))
        else {
            return;
        };
        self.overlay = Some(ClientShellOverlay::ContextMenu(
            ClientContextMenuOverlay::new(
                ClientContextMenuTarget::Tab {
                    tab_id,
                    workspace_id: tab.workspace_id.clone(),
                },
                x,
                y,
            ),
        ));
    }

    pub(super) fn open_pane_context_menu(&mut self, pane_id: String, x: u16, y: u16) {
        let Some(snapshot) = self.snapshot.as_deref() else {
            return;
        };
        let Some(pane) = snapshot.panes.iter().find(|pane| pane.pane_id == pane_id) else {
            return;
        };
        let source_pane_id = snapshot
            .focused_pane_id
            .clone()
            .filter(|focused| focused != &pane_id);
        self.overlay = Some(ClientShellOverlay::ContextMenu(
            ClientContextMenuOverlay::new(
                ClientContextMenuTarget::Pane {
                    pane_id,
                    workspace_id: pane.workspace_id.clone(),
                    source_pane_id,
                    has_manual_label: pane.label.is_some(),
                    right_click_passthrough: pane.right_click_passthrough,
                },
                x,
                y,
            ),
        ));
    }

    /// Moves the highlight `delta` choosable rows, past headers and dividers, stopping at
    /// either end.
    pub(super) fn move_context_menu_selection(&mut self, delta: isize) {
        let Some(ClientShellOverlay::ContextMenu(menu)) = self.overlay.as_mut() else {
            return;
        };
        let items = menu.items();
        let step = delta.signum();
        for _ in 0..delta.unsigned_abs() {
            let mut next = menu.highlighted as isize + step;
            while (0..items.len() as isize).contains(&next) && items[next as usize].action.is_none()
            {
                next += step;
            }
            if !(0..items.len() as isize).contains(&next) {
                return;
            }
            menu.highlighted = next as usize;
        }
    }

    pub(super) fn activate_context_menu_item(
        &mut self,
        index: usize,
        outcome: &mut ClientShellInput,
    ) {
        let Some(ClientShellOverlay::ContextMenu(menu)) = self.overlay.take() else {
            return;
        };
        let row = menu.items().get(index).map(|item| item.action);
        let action = match row {
            Some(Some(action)) => action,
            // A header or divider: nothing to do, the menu stays.
            Some(None) => {
                self.overlay = Some(ClientShellOverlay::ContextMenu(menu));
                return;
            }
            None => {
                outcome.repaint = true;
                return;
            }
        };
        match menu.target {
            ClientContextMenuTarget::Workspace {
                workspace_id,
                close_group,
                ..
            } => self.activate_workspace_context_action(workspace_id, close_group, action, outcome),
            ClientContextMenuTarget::Tab {
                tab_id,
                workspace_id,
            } => self.activate_tab_context_action(tab_id, workspace_id, action, outcome),
            ClientContextMenuTarget::Pane {
                pane_id,
                workspace_id,
                source_pane_id,
                right_click_passthrough,
                ..
            } => self.activate_pane_context_action(
                pane_id,
                workspace_id,
                source_pane_id,
                right_click_passthrough,
                action,
                outcome,
            ),
            ClientContextMenuTarget::WorkItem {
                item_id,
                workspace_id,
                link_target,
                groups,
                links,
                ..
            } => self.activate_work_item_context_action(
                super::work_items::WorkItemContextMenu {
                    item_id,
                    workspace_id,
                    link_target,
                    groups,
                    links,
                },
                action,
                outcome,
            ),
        }
        outcome.repaint = true;
    }

    pub(super) fn activate_workspace_context_action(
        &mut self,
        workspace_id: String,
        close_group: bool,
        action: ClientContextMenuAction,
        outcome: &mut ClientShellInput,
    ) {
        use crate::input::KeybindAction;

        match action {
            ClientContextMenuAction::Rename => {
                let label = self
                    .snapshot
                    .as_deref()
                    .and_then(|snapshot| {
                        snapshot
                            .workspaces
                            .iter()
                            .find(|workspace| workspace.workspace_id == workspace_id)
                    })
                    .map(|workspace| workspace.label.clone());
                if let Some(label) = label {
                    self.overlay = Some(ClientShellOverlay::Rename(ClientRenameOverlay {
                        title: "rename workspace",
                        input: TextEditor::new(&label, false),
                        target: ClientRenameTarget::Workspace { workspace_id },
                    }));
                }
            }
            ClientContextMenuAction::Close => {
                self.request_workspace_close(workspace_id, Some(close_group), outcome);
            }
            ClientContextMenuAction::NewWorktree => {
                self.begin_worktree_action_for(KeybindAction::NewWorktree, workspace_id, outcome)
            }
            ClientContextMenuAction::OpenWorktree => {
                self.begin_worktree_action_for(KeybindAction::OpenWorktree, workspace_id, outcome)
            }
            ClientContextMenuAction::RemoveWorktree => {
                self.begin_worktree_action_for(KeybindAction::RemoveWorktree, workspace_id, outcome)
            }
            ClientContextMenuAction::ToggleGroup => {
                let key = self.snapshot.as_deref().and_then(|snapshot| {
                    snapshot
                        .workspaces
                        .iter()
                        .find(|workspace| workspace.workspace_id == workspace_id)
                        .and_then(|workspace| workspace.worktree.as_ref())
                        .map(|worktree| worktree.key.clone())
                });
                if let Some(key) = key {
                    let endpoint_id = self.active_endpoint_id.clone();
                    self.toggle_collapsed_group(&endpoint_id, key);
                    self.persist_chrome_preferences(outcome);
                }
            }
            _ => {}
        }
    }

    fn activate_tab_context_action(
        &mut self,
        tab_id: String,
        workspace_id: String,
        action: ClientContextMenuAction,
        outcome: &mut ClientShellInput,
    ) {
        use crate::api::schema::{Method, TabTarget};

        self.push_endpoint_method(
            Method::TabFocus(TabTarget {
                tab_id: tab_id.clone(),
            }),
            outcome,
        );
        match action {
            ClientContextMenuAction::NewTab => {
                if self.config.prompt_new_tab_name {
                    let default_name = (self
                        .snapshot
                        .as_deref()
                        .map(|snapshot| {
                            snapshot
                                .tabs
                                .iter()
                                .filter(|tab| tab.workspace_id == workspace_id)
                                .count()
                        })
                        .unwrap_or(0)
                        + 1)
                    .to_string();
                    self.overlay = Some(ClientShellOverlay::Rename(ClientRenameOverlay {
                        title: "new tab",
                        input: TextEditor::new(&default_name, true),
                        target: ClientRenameTarget::NewTab {
                            workspace_id,
                            default_name,
                        },
                    }));
                } else {
                    self.push_endpoint_method(
                        Method::TabCreate(crate::api::schema::TabCreateParams {
                            workspace_id: Some(workspace_id),
                            cwd: None,
                            focus: true,
                            label: None,
                            env: Default::default(),
                        }),
                        outcome,
                    );
                }
            }
            ClientContextMenuAction::Rename => {
                let tab = self
                    .snapshot
                    .as_deref()
                    .and_then(|snapshot| snapshot.tabs.iter().find(|tab| tab.tab_id == tab_id));
                if let Some(tab) = tab {
                    self.overlay = Some(ClientShellOverlay::Rename(ClientRenameOverlay {
                        title: "rename tab",
                        input: TextEditor::new(&tab.label, false),
                        target: ClientRenameTarget::Tab {
                            tab_id,
                            auto_name: !tab.custom_label,
                            original_name: tab.label.clone(),
                        },
                    }));
                }
            }
            ClientContextMenuAction::Close => {
                self.request_tab_close(tab_id, outcome);
            }
            _ => {}
        }
    }

    fn activate_pane_context_action(
        &mut self,
        pane_id: String,
        workspace_id: String,
        source_pane_id: Option<String>,
        right_click_passthrough: bool,
        action: ClientContextMenuAction,
        outcome: &mut ClientShellInput,
    ) {
        use crate::api::schema::{
            Method, PaneInputSetParams, PaneRenameParams, PaneRightClickTarget, PaneSplitParams,
            PaneSwapParams, PaneTarget, PaneZoomMode, PaneZoomParams, SplitDirection,
        };

        match action {
            ClientContextMenuAction::RenamePane => {
                let label = self.snapshot.as_deref().and_then(|snapshot| {
                    snapshot
                        .panes
                        .iter()
                        .find(|pane| pane.pane_id == pane_id)
                        .and_then(|pane| pane.label.clone())
                });
                self.overlay = Some(ClientShellOverlay::Rename(ClientRenameOverlay {
                    title: "rename pane",
                    input: TextEditor::new(label.as_deref().unwrap_or_default(), label.is_none()),
                    target: ClientRenameTarget::Pane { pane_id },
                }));
            }
            ClientContextMenuAction::ClearPaneName => self.push_endpoint_method(
                Method::PaneRename(PaneRenameParams {
                    pane_id,
                    label: None,
                }),
                outcome,
            ),
            ClientContextMenuAction::SwapWithFocusedPane => {
                if let Some(source_pane_id) = source_pane_id {
                    self.push_endpoint_method(
                        Method::PaneSwap(PaneSwapParams {
                            pane_id: None,
                            direction: None,
                            source_pane_id: Some(source_pane_id.clone()),
                            target_pane_id: Some(pane_id),
                        }),
                        outcome,
                    );
                    self.push_endpoint_method(
                        Method::PaneFocus(PaneTarget {
                            pane_id: source_pane_id,
                        }),
                        outcome,
                    );
                }
            }
            ClientContextMenuAction::SplitRight | ClientContextMenuAction::SplitDown => {
                self.push_endpoint_method(
                    Method::PaneSplit(PaneSplitParams {
                        workspace_id: Some(workspace_id),
                        target_pane_id: Some(pane_id),
                        direction: if action == ClientContextMenuAction::SplitRight {
                            SplitDirection::Right
                        } else {
                            SplitDirection::Down
                        },
                        ratio: None,
                        cwd: None,
                        focus: true,
                        right_click: Default::default(),
                        env: Default::default(),
                    }),
                    outcome,
                );
            }
            ClientContextMenuAction::Zoom => self.push_endpoint_method(
                Method::PaneZoom(PaneZoomParams {
                    pane_id: Some(pane_id),
                    mode: PaneZoomMode::Toggle,
                }),
                outcome,
            ),
            ClientContextMenuAction::ToggleRightClickPassthrough => self.push_endpoint_method(
                Method::PaneInputSet(PaneInputSetParams {
                    pane_id,
                    right_click: if right_click_passthrough {
                        PaneRightClickTarget::Herdr
                    } else {
                        PaneRightClickTarget::Pane
                    },
                }),
                outcome,
            ),
            ClientContextMenuAction::ClosePane => {
                self.push_endpoint_method(Method::PaneClose(PaneTarget { pane_id }), outcome)
            }
            _ => {}
        }
    }
}
