//! App-thread driver for work items: schedules polls and preparation on
//! background threads and applies their results.

use std::collections::HashSet;
use std::time::Instant;

use super::{App, AppPolicy};
use crate::events::AppEvent;
use crate::work_items::{StorePolicy, WorkItemNotice, WorkItemsEvent};

pub(super) fn store_policy(policy: AppPolicy) -> StorePolicy {
    StorePolicy {
        path: crate::session::data_dir().join("work-items.json"),
        load: policy.restore_session,
        persist: policy.persist_session,
    }
}

fn send_event(event_tx: &tokio::sync::mpsc::Sender<AppEvent>, event: WorkItemsEvent) {
    let _ = event_tx.blocking_send(AppEvent::WorkItems(Box::new(event)));
}

impl App {
    pub(super) fn apply_work_items_config(&mut self, config: &crate::config::WorkItemsConfig) {
        let existing: HashSet<&str> = self
            .state
            .workspaces
            .iter()
            .map(|ws| ws.id.as_str())
            .collect();
        self.work_items
            .apply_config(config, store_policy(self.policy), &existing, Instant::now());
    }

    pub(super) fn work_items_workspace_closed(&mut self, workspace_id: &str) {
        self.work_items.workspace_closed(workspace_id);
    }

    /// Starts due polls. Returns whether visible state changed.
    pub(crate) fn run_work_items_tasks(&mut self, now: Instant) -> bool {
        if !self.work_items.is_enabled() {
            return false;
        }
        for source in self.work_items.take_due_polls(now) {
            let event_tx = self.event_tx.clone();
            std::thread::spawn(move || {
                let result = source.poll();
                send_event(
                    &event_tx,
                    WorkItemsEvent::Polled {
                        source_id: source.id().to_string(),
                        result,
                    },
                );
            });
        }
        false
    }

    pub(crate) fn handle_work_items_event(
        &mut self,
        event: WorkItemsEvent,
    ) -> (bool, Vec<WorkItemNotice>) {
        let polled_source = match &event {
            WorkItemsEvent::Polled {
                source_id,
                result: Ok(_),
            } => Some(source_id.clone()),
            _ => None,
        };
        let (changed, notices) = self.work_items.apply_event(event, Instant::now());
        if let Some(source_id) = polled_source {
            self.start_work_items_prepare(&source_id);
        }
        if changed {
            self.render_dirty.request_generic();
            self.render_notify.notify_one();
        }
        (changed, notices)
    }

    fn start_work_items_prepare(&mut self, source_id: &str) {
        let Some(source) = self.work_items.source(source_id).cloned() else {
            return;
        };
        let pending = self.work_items.take_needs_prepare(source_id);
        if pending.is_empty() {
            return;
        }
        let event_tx = self.event_tx.clone();
        std::thread::spawn(move || {
            for item in pending {
                let prepared = source.prepare(&item);
                send_event(
                    &event_tx,
                    WorkItemsEvent::Prepared {
                        key: crate::work_items::state::item_key(source.id(), &item.external_id),
                        updated_at: item.updated_at,
                        prepared,
                    },
                );
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use crate::api::schema::{
        EmptyParams, ErrorResponse, Method, Request, ResponseResult, SuccessResponse,
        WorkItemChooseParams, WorkItemInfo, WorkItemPhase,
    };
    use crate::app::{App, AppPolicy};
    use crate::work_items::test_support::{source_item, FakeSource};
    use crate::work_items::WorkItems;

    fn test_app() -> App {
        App::new(
            &crate::config::Config::default(),
            AppPolicy::TEST,
            None,
            tokio::sync::mpsc::unbounded_channel().1,
            crate::api::EventHub::default(),
        )
    }

    fn request(method: Method) -> Request {
        Request {
            id: "test".into(),
            method,
        }
    }

    fn list(app: &mut App) -> Vec<WorkItemInfo> {
        let response =
            app.handle_api_request(request(Method::WorkItemList(EmptyParams::default())));
        let success: SuccessResponse = serde_json::from_str(&response).expect("list succeeds");
        let ResponseResult::WorkItemList { items, .. } = success.result else {
            panic!("expected work item list, got {response}");
        };
        items
    }

    /// Runs due tasks and drains events until `done` holds or two seconds pass.
    fn run_until(app: &mut App, mut done: impl FnMut(&mut App) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(2);
        app.run_work_items_tasks(Instant::now());
        while Instant::now() < deadline {
            app.drain_all_internal_events();
            if done(app) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("condition not reached within 2 s");
    }

    #[test]
    fn default_config_schedules_nothing_and_reports_disabled() {
        let mut app = test_app();
        assert_eq!(app.work_items.next_deadline(), None);
        assert!(!app.run_work_items_tasks(Instant::now()));
        assert!(app.event_rx.try_recv().is_err());
        let response =
            app.handle_api_request(request(Method::WorkItemList(EmptyParams::default())));
        let error: ErrorResponse = serde_json::from_str(&response).expect("error response");
        assert_eq!(error.error.code, "work_items_disabled");
    }

    #[test]
    fn first_poll_runs_immediately_and_prepares_new_items() {
        let mut app = test_app();
        let source = FakeSource::with_items(vec![source_item("a")]);
        app.work_items = WorkItems::for_test(vec![source.clone()], Instant::now());
        run_until(&mut app, |app| {
            list(app).first().is_some_and(|item| item.summary.is_some())
        });
        let items = list(&mut app);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].item_id, "fake:a");
        assert!(!items[0].seen);
        assert_eq!(items[0].phase, WorkItemPhase::Pending);
        assert_eq!(source.prepare_calls(), 1);
    }

    #[test]
    fn external_choice_awaits_until_the_source_drops_the_item() {
        let mut app = test_app();
        let source = FakeSource::with_items(vec![source_item("a")]);
        app.work_items = WorkItems::for_test(vec![source.clone() as Arc<_>], Instant::now());
        run_until(&mut app, |app| !list(app).is_empty());

        let response =
            app.handle_api_request(request(Method::WorkItemChoose(WorkItemChooseParams {
                item_id: "fake:a".into(),
                choice_id: "web".into(),
            })));
        assert!(
            serde_json::from_str::<SuccessResponse>(&response).is_ok(),
            "{response}"
        );
        let items = list(&mut app);
        assert_eq!(items[0].phase, WorkItemPhase::AwaitingExternal);
        assert!(items[0].seen);

        source.set_items(Vec::new());
        app.work_items.schedule_all_for_test(Instant::now());
        run_until(&mut app, |app| list(app).is_empty());
    }
}
