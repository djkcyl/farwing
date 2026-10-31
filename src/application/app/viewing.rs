//! Local connection identity before and after an assistance target is resolved.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) enum Key {
    Device(String),
    Assistance(String),
    /// A host reached directly, by address.
    Direct(String),
    /// A device's cameras only (next to a window of its desktop).
    Camera(String),
}

pub(super) struct Session {
    pub key: Key,
    pub generation: u64,
    pub device_id: Option<String>,
    pub alias: String,
    pub handle: crate::session::controller::windows::ViewerHandle,
    pub closing: bool,
}
#[derive(Default)]
pub(super) struct Registry {
    pub active: Vec<Session>,
    pub opening: std::collections::BTreeMap<Key, Pending>,
}
pub(super) struct Pending {
    generation: u64,
    cancelled: bool,
}
impl Registry {
    pub fn has_viewers(&self) -> bool {
        !self.active.is_empty() || !self.opening.is_empty()
    }
    pub fn begin(&mut self, key: Key, generation: u64) -> bool {
        if self.opening.contains_key(&key)
            || self.active.iter().any(|s| {
                s.key == key
                    || (!s.key.is_camera()
                        && key
                            .device()
                            .is_some_and(|id| s.device_id.as_deref() == Some(id)))
            })
        {
            return false;
        }
        self.opening.insert(
            key,
            Pending {
                generation,
                cancelled: false,
            },
        );
        true
    }
    pub fn completed_open(&mut self, key: &Key, generation: u64) -> bool {
        if self
            .opening
            .get(key)
            .is_none_or(|pending| pending.generation != generation)
        {
            return false;
        }
        !self.opening.remove(key).unwrap().cancelled
    }
    /// The desktop (or listening) window of device `id`.
    pub fn for_device(&self, id: &str) -> Option<&Session> {
        self.active
            .iter()
            .find(|s| !s.key.is_camera() && s.device_id.as_deref() == Some(id))
    }
    pub fn waits_for_device(&self, id: &str) -> bool {
        self.active
            .iter()
            .any(|s| s.device_id.as_deref().is_none_or(|target| target == id))
            || self
                .opening
                .keys()
                .any(|key| key.device().is_none_or(|target| target == id))
    }
    pub fn stop_device(&mut self, id: &str) {
        for (key, pending) in &mut self.opening {
            if key.device() == Some(id) || key.camera_device() == Some(id) {
                pending.cancelled = true;
            }
        }
        for session in &mut self.active {
            if session.device_id.as_deref() == Some(id) {
                session.handle.request_close();
                session.closing = true;
            }
        }
    }
    pub fn stop_all(&mut self) {
        for pending in self.opening.values_mut() {
            pending.cancelled = true;
        }
        for session in &mut self.active {
            session.handle.request_close();
            session.closing = true;
        }
    }
    pub fn finished(
        &mut self,
    ) -> Vec<(
        u64,
        String,
        std::result::Result<crate::session::controller::windows::ViewerEnd, String>,
    )> {
        let mut finished = Vec::new();
        self.active.retain_mut(|session| {
            if let Some(target) = session.handle.info().and_then(|info| info.target) {
                session.device_id = Some(target.device_id);
                session.alias = target.alias;
            }
            if let Some(result) = session.handle.result() {
                finished.push((session.generation, session.alias.clone(), result));
                false
            } else {
                true
            }
        });
        finished
    }
}
impl Key {
    pub fn new(
        device: Option<&str>,
        assist: Option<&crate::account::assist::AssistRequest>,
    ) -> Option<Self> {
        device
            .map(|id| Self::Device(id.to_owned()))
            .or_else(|| assist.map(|a| Self::Assistance(a.target.clone())))
    }
    pub fn device(&self) -> Option<&str> {
        match self {
            Self::Device(id) => Some(id),
            Self::Assistance(_) | Self::Direct(_) | Self::Camera(_) => None,
        }
    }
    /// The same device's cameras-only window.
    pub fn camera(self) -> Self {
        match self {
            Self::Device(id) => Self::Camera(id),
            other => other,
        }
    }
    pub fn is_camera(&self) -> bool {
        matches!(self, Self::Camera(_))
    }
    fn camera_device(&self) -> Option<&str> {
        match self {
            Self::Camera(id) => Some(id),
            _ => None,
        }
    }
}
