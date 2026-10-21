//! Windows toast delivery, activation registration and notification placement.

use crate::i18n::t;
use anyhow::{Context, Result};
mod placement;
mod registration;
pub(crate) use placement::Placement;
pub(crate) use registration::{owns_shortcut, unregister};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock, mpsc};
use windows::Data::Xml::Dom::XmlDocument;
use windows::Foundation::{DateTime, IReference, PropertyValue};
use windows::UI::Notifications::{
    NotificationData, NotificationSetting, NotificationUpdateResult, ToastNotification,
    ToastNotificationManager, ToastNotifier,
};
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize};
use windows::core::{HSTRING, Interface};
const GROUP: &str = "remote-access";

type Action = crate::application::app::notifications::Action;
fn activation_sink() -> &'static Mutex<Option<mpsc::Sender<Action>>> {
    static SINK: OnceLock<Mutex<Option<mpsc::Sender<Action>>>> = OnceLock::new();
    SINK.get_or_init(Mutex::default)
}
pub(crate) fn set_activation_sink(sender: Option<mpsc::Sender<Action>>) {
    *activation_sink().lock().unwrap_or_else(|p| p.into_inner()) = sender;
}
pub(crate) fn receive_activation(uri: &str) -> bool {
    let Ok(action) = Action::parse(uri) else {
        return false;
    };
    activation_sink()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .as_ref()
        .is_some_and(|s| s.send(action).is_ok())
}
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ToastAction {
    pub label: String,
    pub uri: String,
}
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Toast {
    pub progress: Option<u16>,
    pub key: String,
    pub title: String,
    pub body: String,
    pub expires_at: i64,
    pub buttons: Vec<ToastAction>,
}
fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
fn xml(toast: &Toast) -> String {
    let uri = crate::application::app::notifications::Action::uri(
        &toast.key,
        crate::application::app::notifications::Verb::Open,
    );
    let actions = toast
        .buttons
        .iter()
        .map(|a| {
            format!(
                "<action content=\"{}\" arguments=\"{}\" activationType=\"protocol\"/>",
                escape(&a.label),
                escape(&a.uri)
            )
        })
        .collect::<String>();
    let progress = if toast.progress.is_some() {
        r#"<progress value="{progress}" status="{status}"/>"#
    } else {
        ""
    };
    let title = if toast.progress.is_some() {
        "{title}".to_owned()
    } else {
        escape(&toast.title)
    };
    let body = if toast.progress.is_some() {
        "{body}".to_owned()
    } else {
        escape(&toast.body)
    };
    format!(
        "<toast activationType=\"protocol\" launch=\"{}\" duration=\"long\"><visual><binding template=\"ToastGeneric\"><text>{}</text><text>{}</text>{progress}</binding></visual><actions>{actions}</actions><audio silent=\"true\"/></toast>",
        escape(&uri),
        title,
        body
    )
}
struct Apartment(std::marker::PhantomData<std::rc::Rc<()>>);
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe { RoUninitialize() }
    }
}
pub(crate) struct Native {
    sequence: u32,
    notifier: ToastNotifier,
    shown: HashMap<String, (Toast, ToastNotification)>,
    _apartment: Apartment,
}
impl Native {
    pub fn new() -> Result<Self> {
        unsafe {
            RoInitialize(RO_INIT_MULTITHREADED)?;
        }
        let apartment = Apartment(Default::default());
        let aumid = crate::product::aumid();
        registration::register(&aumid).context(t().system.notification_register_failed)?;
        let notifier = ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(&aumid))
            .context(t().system.notification_create_failed)?;

        Ok(Self {
            sequence: 0,
            notifier,
            shown: HashMap::new(),
            _apartment: apartment,
        })
    }
    pub fn synchronize(&mut self, items: &[Toast]) -> Result<()> {
        let remove = self
            .shown
            .keys()
            .filter(|key| !items.iter().any(|i| &i.key == *key))
            .cloned()
            .collect::<Vec<_>>();
        for key in remove {
            self.remove(&key)?;
        }
        for item in items {
            if self
                .shown
                .get(&item.key)
                .is_some_and(|(old, _)| old == item)
            {
                continue;
            }
            anyhow::ensure!(
                item.key.len() == 32 && item.key.bytes().all(|b| b.is_ascii_hexdigit()),
                "{}",
                t().system.notification_invalid_key
            );
            if item.progress.is_some()
                && self
                    .shown
                    .get(&item.key)
                    .is_some_and(|(old, _)| old.progress.is_some())
            {
                let data = self.progress_data(item)?;
                let result = self.notifier.UpdateWithTagAndGroup(
                    &data,
                    &HSTRING::from(&item.key[..16]),
                    &HSTRING::from(GROUP),
                )?;
                anyhow::ensure!(
                    result != NotificationUpdateResult::Failed,
                    "{}",
                    t().system.notification_progress_failed
                );
                // A dismissed progress toast stays dismissed. The terminal state
                // has a separate ticket and may notify once when it arrives.
                let actions_changed = self
                    .shown
                    .get(&item.key)
                    .is_some_and(|(old, _)| old.buttons != item.buttons);
                if result == NotificationUpdateResult::NotificationNotFound || !actions_changed {
                    if let Some((old, _)) = self.shown.get_mut(&item.key) {
                        *old = item.clone();
                    }
                    continue;
                }
                // Data binding cannot change action buttons. Replace a still-present
                // toast silently when its available actions change; never revive a dismissed one.
            }
            let document = XmlDocument::new()?;
            document.LoadXml(&HSTRING::from(xml(item)))?;
            let notification = ToastNotification::CreateToastNotification(&document)?;
            notification.SetTag(&HSTRING::from(&item.key[..16]))?;
            notification.SetGroup(&HSTRING::from(GROUP))?;
            if item.progress.is_some() {
                notification.SetData(&self.progress_data(item)?)?;
            }
            notification.SetSuppressPopup(self.shown.contains_key(&item.key))?;
            if item.expires_at > 0 {
                let date = DateTime {
                    UniversalTime: (item.expires_at + 11_644_473_600) * 10_000_000,
                };
                let value: IReference<DateTime> = PropertyValue::CreateDateTime(date)?.cast()?;
                notification.SetExpirationTime(&value)?;
            }
            // A newly registered unpackaged identity has no notification settings
            // entry until its first Show. Windows still applies global policies.
            match self.notifier.Setting() {
                Ok(value) => anyhow::ensure!(
                    value == NotificationSetting::Enabled,
                    "{}",
                    t().system.notification_disabled
                ),
                Err(error) if error.code() == windows::core::HRESULT::from_win32(1168) => (),
                Err(error) => return Err(error).context(t().system.notification_setting_failed),
            }
            self.notifier
                .Show(&notification)
                .context(t().system.notification_show_failed)?;
            self.shown
                .insert(item.key.clone(), (item.clone(), notification));
        }
        Ok(())
    }
    fn progress_data(&mut self, item: &Toast) -> Result<NotificationData> {
        self.sequence = self.sequence.wrapping_add(1).max(1);
        let data = NotificationData::new()?;
        data.SetSequenceNumber(self.sequence)?;
        let values = data.Values()?;
        values.Insert(&HSTRING::from("title"), &HSTRING::from(&item.title))?;
        values.Insert(&HSTRING::from("body"), &HSTRING::from(&item.body))?;
        values.Insert(
            &HSTRING::from("progress"),
            &HSTRING::from(format!(
                "{:.3}",
                f64::from(item.progress.unwrap_or(0).min(1000)) / 1000.
            )),
        )?;
        values.Insert(
            &HSTRING::from("status"),
            &HSTRING::from(t().system.notification_transferring),
        )?;
        Ok(data)
    }
    fn remove(&mut self, key: &str) -> Result<()> {
        if let Some((_, notification)) = self.shown.get(key) {
            self.notifier.Hide(notification)?;
        }
        ToastNotificationManager::History()?.RemoveGroupedTagWithId(
            &HSTRING::from(&key[..16]),
            &HSTRING::from(GROUP),
            &HSTRING::from(crate::product::aumid()),
        )?;
        self.shown.remove(key);
        Ok(())
    }
    pub fn clear(&mut self) -> Result<()> {
        let keys = self.shown.keys().cloned().collect::<Vec<_>>();
        for key in keys {
            self.remove(&key)?;
        }
        Ok(())
    }
}
