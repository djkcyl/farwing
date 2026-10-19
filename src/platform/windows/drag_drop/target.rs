//! Apartment-owned OLE drop registration. It copies path metadata only; callers
//! enumerate/read the selected files outside Windows' drag callbacks.
use anyhow::{Result, ensure};
use std::{
    cell::{Cell, RefCell},
    path::PathBuf,
    rc::Rc,
};
use windows::{
    Win32::{
        Foundation::*,
        System::{Com::*, Memory::GlobalSize, Ole::*, SystemServices::MODIFIERKEYS_FLAGS},
        UI::Shell::{CLSID_DragDropHelper, DragQueryFileW, HDROP, IDropTargetHelper},
    },
    core::{Ref, implement},
};

pub(crate) trait Handler {
    fn description(&self) -> Option<&'static str> {
        None
    }
    fn reenter(&mut self, _identity: super::appearance::Identity, _point: POINTL) -> Option<u32> {
        None
    }
    fn enter(&mut self, paths: Vec<PathBuf>, point: POINTL) -> u32;
    fn over(&mut self, point: POINTL) -> u32;
    fn leave(&mut self);
    fn drop_at(&mut self, point: POINTL) -> u32;
}

pub(crate) struct Registration {
    hwnd: HWND,
    _target: Option<IDropTarget>,
    handler: Rc<RefCell<dyn Handler>>,
}
impl Registration {
    /// The caller must own this HWND and have disabled any framework drop handler.
    pub fn new(hwnd: HWND, handler: Rc<RefCell<dyn Handler>>) -> Result<Self> {
        unsafe {
            OleInitialize(None)?;
        }
        let target: IDropTarget = Target {
            handler: handler.clone(),
            hwnd,
            data: RefCell::new(None),
            drag_identity: Cell::new(None),
            description_effect: Cell::new(None),
            helper: unsafe { CoCreateInstance(&CLSID_DragDropHelper, None, CLSCTX_INPROC_SERVER) }
                .ok(),
        }
        .into();
        if let Err(error) = unsafe { RegisterDragDrop(hwnd, &target) } {
            unsafe {
                OleUninitialize();
            }
            return Err(error.into());
        }
        Ok(Self {
            hwnd,
            _target: Some(target),
            handler,
        })
    }
}
impl Drop for Registration {
    fn drop(&mut self) {
        if let Ok(mut handler) = self.handler.try_borrow_mut() {
            handler.leave();
        }
        unsafe {
            let _ = RevokeDragDrop(self.hwnd);
        }
        self._target.take();
        unsafe {
            OleUninitialize();
        }
    }
}
#[implement(IDropTarget, Agile = false)]
struct Target {
    handler: Rc<RefCell<dyn Handler>>,
    hwnd: HWND,
    helper: Option<IDropTargetHelper>,
    data: RefCell<Option<IDataObject>>,
    drag_identity: Cell<Option<super::appearance::Identity>>,
    description_effect: Cell<Option<u32>>,
}
impl Target {
    fn describe(&self, effect: u32) {
        let text = self.handler.try_borrow().ok().and_then(|h| h.description());
        if text.is_none() || self.description_effect.replace(Some(effect)) == Some(effect) {
            return;
        }
        let data = self.data.borrow().clone();
        if let Some(data) = data {
            let _ = super::appearance::description(
                &data,
                effect,
                Some(if effect & DROPEFFECT_COPY.0 != 0 {
                    text.unwrap()
                } else {
                    "无法复制到此位置"
                }),
            );
        }
    }
    fn apply(&self, effect: *mut DROPEFFECT, operation: impl FnOnce(&mut dyn Handler) -> u32) {
        if effect.is_null() {
            return;
        }
        let permitted = unsafe { (*effect).0 } & DROPEFFECT_COPY.0;
        if permitted == 0 {
            unsafe {
                *effect = DROPEFFECT_NONE;
            }
            return;
        }
        let value = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.handler
                .try_borrow_mut()
                .map_or(0, |mut h| operation(&mut *h))
        }))
        .unwrap_or(0);
        unsafe {
            *effect = DROPEFFECT(value & permitted);
        }
    }
}
impl IDropTarget_Impl for Target_Impl {
    fn DragEnter(
        &self,
        data: Ref<'_, IDataObject>,
        _: MODIFIERKEYS_FLAGS,
        point: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        let retired = std::mem::replace(&mut *self.data.borrow_mut(), data.as_ref().cloned());
        drop(retired);
        self.description_effect.set(None);
        self.drag_identity
            .set(data.as_ref().and_then(super::appearance::identity));
        self.apply(effect, |handler| {
            handler.leave();
            if let Some(identity) = self.drag_identity.get()
                && let Some(effect) = handler.reenter(identity, *point)
            {
                return effect;
            }
            match data.as_ref().map(paths).transpose() {
                Ok(Some(paths)) => handler.enter(paths, *point),
                Ok(None) => 0,
                Err(error) => {
                    tracing::debug!(%error, "drag source is not an available file selection");
                    0
                }
            }
        });
        if !effect.is_null() {
            self.describe(unsafe { (*effect).0 });
        }
        if let (Some(helper), Some(data)) = (&self.helper, data.as_ref())
            && !effect.is_null()
        {
            unsafe {
                let _ = helper.DragEnter(
                    self.hwnd,
                    data,
                    &POINT {
                        x: point.x,
                        y: point.y,
                    },
                    *effect,
                );
            }
        }
        Ok(())
    }
    fn DragOver(
        &self,
        _: MODIFIERKEYS_FLAGS,
        point: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        self.apply(effect, |handler| {
            // Enter can hit a resize edge, title bar or letterbox. Keep the
            // identity for the whole hover so moving into video can hand back
            // the original gesture, without parsing it as a new upload.
            if let Some(identity) = self.drag_identity.get()
                && let Some(effect) = handler.reenter(identity, *point)
            {
                return effect;
            }
            handler.over(*point)
        });
        if !effect.is_null() {
            self.describe(unsafe { (*effect).0 });
        }
        if let Some(helper) = &self.helper
            && !effect.is_null()
        {
            unsafe {
                let _ = helper.DragOver(
                    &POINT {
                        x: point.x,
                        y: point.y,
                    },
                    *effect,
                );
            }
        }
        Ok(())
    }
    fn DragLeave(&self) -> windows::core::Result<()> {
        self.drag_identity.set(None);
        let data = self.data.borrow_mut().take();
        if self.description_effect.take().is_some()
            && let Some(data) = data
        {
            let _ = super::appearance::description(&data, 0, None);
        }
        let mut effect = DROPEFFECT_COPY;
        self.apply(&mut effect, |handler| {
            handler.leave();
            0
        });
        if let Some(helper) = &self.helper {
            unsafe {
                let _ = helper.DragLeave();
            }
        }
        Ok(())
    }
    fn Drop(
        &self,
        data: Ref<'_, IDataObject>,
        _: MODIFIERKEYS_FLAGS,
        point: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        self.drag_identity.set(None);
        self.apply(effect, |handler| handler.drop_at(*point));
        if !effect.is_null() {
            self.describe(unsafe { (*effect).0 });
        }
        if let (Some(helper), Some(data)) = (&self.helper, data.as_ref())
            && !effect.is_null()
        {
            unsafe {
                let _ = helper.Drop(
                    data,
                    &POINT {
                        x: point.x,
                        y: point.y,
                    },
                    *effect,
                );
            }
        }
        let retired = self.data.borrow_mut().take();
        drop(retired);
        self.description_effect.set(None);
        Ok(())
    }
}
fn paths(data: &IDataObject) -> Result<Vec<PathBuf>> {
    let format = FORMATETC {
        cfFormat: 15,
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
        ..Default::default()
    };
    let mut medium = unsafe { data.GetData(&format)? };
    struct Medium<'a>(&'a mut STGMEDIUM);
    impl Drop for Medium<'_> {
        fn drop(&mut self) {
            unsafe {
                ReleaseStgMedium(self.0);
            }
        }
    }
    let medium = Medium(&mut medium);
    ensure!(
        medium.0.tymed == TYMED_HGLOBAL.0 as u32,
        "文件路径格式不正确"
    );
    let memory = unsafe { medium.0.u.hGlobal };
    let bytes = unsafe { GlobalSize(memory) };
    ensure!(
        (20..=16 * 1024 * 1024).contains(&bytes),
        "文件选择过大或无效"
    );
    let drop = HDROP(memory.0);
    let count = unsafe { DragQueryFileW(drop, u32::MAX, None) };
    ensure!((1..=16384).contains(&count), "文件选择数量超限");
    let mut result = Vec::with_capacity(count as usize);
    let mut total = 0usize;
    for index in 0..count {
        let length = unsafe { DragQueryFileW(drop, index, None) } as usize;
        total = total.saturating_add(length);
        ensure!(
            length > 0 && length <= 32767 && total <= 8 * 1024 * 1024,
            "文件路径过长"
        );
        let mut text = vec![0u16; length + 1];
        ensure!(
            unsafe { DragQueryFileW(drop, index, Some(&mut text)) } as usize == length,
            "读取文件路径失败"
        );
        use std::os::windows::ffi::OsStringExt;
        let path = PathBuf::from(std::ffi::OsString::from_wide(&text[..length]));
        ensure!(path.is_absolute(), "拖放需要完整文件路径");
        result.push(path);
    }
    Ok(result)
}
