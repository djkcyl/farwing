use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ConnectionTabState {
    Connecting { detail: String },
    Ready,
    Failed { detail: String },
}

impl ConnectionTabState {
    fn ready(&self) -> bool {
        matches!(self, Self::Ready)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConnectionSelection {
    Ignore,
    Defer,
    Activate,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ConnectionTabCommand {
    Select(String),
    Close(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ConnectionTab {
    pub key: String,
    pub alias: String,
    pub state: ConnectionTabState,
}

#[derive(Default)]
struct ConnectionTabsState {
    tabs: Vec<ConnectionTab>,
    active: Option<String>,
    command: Option<ConnectionTabCommand>,
}

/// Shared model for the device tabs rendered by the UI thread.
///
/// The event-loop owner and the active player can both reach this model. The
/// lock is deliberately small-lived: no renderer or network operation is
/// performed while holding it.
#[derive(Clone, Default)]
pub(crate) struct ConnectionTabs {
    state: Arc<Mutex<ConnectionTabsState>>,
}

impl std::fmt::Debug for ConnectionTabs {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ConnectionTabs")
            .field("tabs", &self.snapshot())
            .field("active", &self.active_key())
            .finish()
    }
}

impl ConnectionTabs {
    pub fn add(&self, key: impl Into<String>, alias: impl Into<String>) -> bool {
        let key = key.into();
        let alias = alias.into();
        let mut state = self.lock();
        if let Some(tab) = state.tabs.iter_mut().find(|tab| tab.key == key) {
            tab.alias = alias;
            return false;
        }
        if state.active.is_none() {
            state.active = Some(key.clone());
        }
        state.tabs.push(ConnectionTab {
            key,
            alias,
            state: ConnectionTabState::Connecting {
                detail: "正在连接".to_owned(),
            },
        });
        true
    }

    pub fn set_ready(&self, key: &str) -> bool {
        let mut state = self.lock();
        let Some(tab) = state.tabs.iter_mut().find(|tab| tab.key == key) else {
            return false;
        };
        tab.state = ConnectionTabState::Ready;
        if state.active.is_none() {
            state.active = Some(key.to_owned());
        }
        true
    }

    pub fn set_alias(&self, key: &str, alias: impl Into<String>) -> bool {
        let mut state = self.lock();
        let Some(tab) = state.tabs.iter_mut().find(|tab| tab.key == key) else {
            return false;
        };
        tab.alias = alias.into();
        true
    }

    pub fn set_progress(&self, key: &str, detail: impl Into<String>) -> bool {
        let mut state = self.lock();
        let Some(tab) = state.tabs.iter_mut().find(|tab| tab.key == key) else {
            return false;
        };
        tab.state = ConnectionTabState::Connecting {
            detail: detail.into(),
        };
        true
    }

    pub fn set_failed(&self, key: &str, detail: impl Into<String>) -> bool {
        let mut state = self.lock();
        let Some(tab) = state.tabs.iter_mut().find(|tab| tab.key == key) else {
            return false;
        };
        tab.state = ConnectionTabState::Failed {
            detail: detail.into(),
        };
        true
    }

    pub fn selection_action(&self, key: &str, stored_session: bool) -> ConnectionSelection {
        let state = self.lock();
        let Some(tab) = state.tabs.iter().find(|tab| tab.key == key) else {
            return ConnectionSelection::Ignore;
        };
        if matches!(tab.state, ConnectionTabState::Failed { .. }) {
            ConnectionSelection::Ignore
        } else if stored_session {
            ConnectionSelection::Activate
        } else {
            ConnectionSelection::Defer
        }
    }

    pub fn failure_detail(&self, key: &str) -> Option<String> {
        let state = self.lock();
        let tab = state.tabs.iter().find(|tab| tab.key == key)?;
        match &tab.state {
            ConnectionTabState::Failed { detail } => Some(detail.clone()),
            _ => None,
        }
    }

    pub fn select(&self, key: &str) -> bool {
        let mut state = self.lock();
        if state
            .tabs
            .iter()
            .find(|tab| tab.key == key)
            .is_some_and(|tab| tab.state.ready())
        {
            state.active = Some(key.to_owned());
            true
        } else {
            false
        }
    }

    pub fn request_close(&self, key: impl Into<String>) {
        self.lock().command = Some(ConnectionTabCommand::Close(key.into()));
    }

    pub fn remove(&self, key: &str) -> Option<String> {
        let mut state = self.lock();
        let index = state.tabs.iter().position(|tab| tab.key == key)?;
        let was_active = state.active.as_deref() == Some(key);
        state.tabs.remove(index);
        if !was_active {
            return state.active.clone();
        }
        let nearest_ready = state
            .tabs
            .iter()
            .enumerate()
            .filter(|(_, tab)| tab.state.ready())
            .min_by_key(|(candidate, _)| {
                (candidate.abs_diff(index), usize::from(*candidate < index))
            })
            .map(|(_, tab)| tab.key.clone());
        let next = nearest_ready.or_else(|| {
            state
                .tabs
                .get(index)
                .or_else(|| {
                    index
                        .checked_sub(1)
                        .and_then(|previous| state.tabs.get(previous))
                })
                .map(|tab| tab.key.clone())
        });
        state.active = next.clone();
        next
    }

    pub fn active_key(&self) -> Option<String> {
        self.lock().active.clone()
    }

    #[cfg(test)]
    pub fn contains(&self, key: &str) -> bool {
        self.lock().tabs.iter().any(|tab| tab.key == key)
    }

    pub fn snapshot(&self) -> Vec<ConnectionTab> {
        self.lock().tabs.clone()
    }

    pub fn len(&self) -> usize {
        self.lock().tabs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lock().tabs.is_empty()
    }

    pub fn take_command(&self) -> Option<ConnectionTabCommand> {
        self.lock().command.take()
    }

    pub fn request_select(&self, key: impl Into<String>) {
        self.lock().command = Some(ConnectionTabCommand::Select(key.into()));
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ConnectionTabsState> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::{ConnectionSelection, ConnectionTabCommand, ConnectionTabState, ConnectionTabs};

    #[test]
    fn ready_tabs_can_be_switched_without_dropping_other_sessions() {
        let tabs = ConnectionTabs::default();
        assert!(tabs.add("one", "工作电脑"));
        assert!(tabs.set_ready("one"));
        assert_eq!(tabs.active_key().as_deref(), Some("one"));
        assert!(tabs.add("two", "家用电脑"));
        assert_eq!(tabs.active_key().as_deref(), Some("one"));
        assert!(
            !tabs.select("two"),
            "a connecting tab cannot replace a working session"
        );
        assert!(tabs.set_ready("two"));
        assert!(tabs.select("two"));
        assert!(tabs.select("one"));
        assert!(tabs.select("two"));
        assert_eq!(
            tabs.snapshot().len(),
            2,
            "switching keeps both sessions in the workspace"
        );
    }

    #[test]
    fn closing_active_tab_falls_back_to_nearest_ready_tab() {
        let tabs = ConnectionTabs::default();
        for (key, alias) in [("one", "One"), ("two", "Two"), ("three", "Three")] {
            assert!(tabs.add(key, alias));
            assert!(tabs.set_ready(key));
        }
        assert!(tabs.select("two"));
        assert_eq!(tabs.remove("two").as_deref(), Some("three"));
        assert_eq!(tabs.active_key().as_deref(), Some("three"));
        assert!(tabs.contains("one"));
        assert!(!tabs.contains("two"));
    }

    #[test]
    fn closing_last_ready_tab_activates_the_remaining_connecting_tab() {
        let tabs = ConnectionTabs::default();
        assert!(tabs.add("ready", "Ready"));
        assert!(tabs.set_ready("ready"));
        assert!(tabs.add("pending", "Pending"));
        assert_eq!(tabs.remove("ready").as_deref(), Some("pending"));
        assert_eq!(tabs.active_key().as_deref(), Some("pending"));
        assert!(matches!(
            tabs.snapshot().first().map(|tab| &tab.state),
            Some(ConnectionTabState::Connecting { .. })
        ));
    }

    #[test]
    fn closing_active_prefers_a_ready_tab_over_a_nearer_pending_tab() {
        let tabs = ConnectionTabs::default();
        assert!(tabs.add("first", "First"));
        assert!(tabs.set_ready("first"));
        assert!(tabs.add("active", "Active"));
        assert!(tabs.set_ready("active"));
        assert!(tabs.add("pending", "Pending"));
        assert!(tabs.select("active"));

        assert_eq!(tabs.remove("active").as_deref(), Some("first"));
        assert_eq!(tabs.active_key().as_deref(), Some("first"));
    }

    #[test]
    fn failing_active_tab_stays_selected_until_its_close_is_processed() {
        let tabs = ConnectionTabs::default();
        assert!(tabs.add("active", "Active"));
        assert!(tabs.set_ready("active"));
        assert!(tabs.add("other", "Other"));
        assert!(tabs.set_ready("other"));
        assert!(tabs.select("active"));

        assert!(tabs.set_failed("active", "disconnected"));
        assert_eq!(tabs.active_key().as_deref(), Some("active"));
    }

    #[test]
    fn selection_waits_for_cached_session_and_ignores_failed_tabs() {
        let tabs = ConnectionTabs::default();
        assert!(tabs.add("pending", "Pending"));
        assert_eq!(
            tabs.selection_action("pending", false),
            ConnectionSelection::Defer
        );
        assert!(tabs.set_ready("pending"));
        assert_eq!(
            tabs.selection_action("pending", false),
            ConnectionSelection::Defer,
            "progress can report Ready before the video session reaches the UI"
        );
        assert_eq!(
            tabs.selection_action("pending", true),
            ConnectionSelection::Activate
        );
        assert!(tabs.set_failed("pending", "failed"));
        assert_eq!(
            tabs.selection_action("pending", true),
            ConnectionSelection::Ignore
        );
    }

    #[test]
    fn failed_tab_exposes_its_connection_error_for_feedback() {
        let tabs = ConnectionTabs::default();
        assert!(tabs.add("camera", "监控02"));
        assert!(tabs.set_failed("camera", "设备ID或验证码错误"));

        assert_eq!(
            tabs.failure_detail("camera"),
            Some("设备ID或验证码错误".into())
        );
        assert_eq!(tabs.failure_detail("missing"), None);
    }

    #[test]
    fn close_command_is_delivered_once() {
        let tabs = ConnectionTabs::default();
        tabs.request_close("one");
        assert_eq!(
            tabs.take_command(),
            Some(ConnectionTabCommand::Close("one".into()))
        );
        assert_eq!(tabs.take_command(), None);
    }

    #[test]
    fn duplicate_key_updates_label_without_duplicating_tab() {
        let tabs = ConnectionTabs::default();
        assert!(tabs.add("device-1", "Old label"));
        assert!(!tabs.add("device-1", "New label"));
        assert_eq!(tabs.snapshot().len(), 1);
        assert_eq!(tabs.snapshot()[0].alias, "New label");
    }
}
