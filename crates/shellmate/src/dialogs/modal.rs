//! Shellmate modals share GPUI's window-local stack. When a prompt moves, keep the
//! builders and focus of any modals above it while removing its old layer.
use gpui::{
    App, Global, Window, WindowId,
    component::{WindowExt as _, dialog::Dialog},
};
use std::{
    cell::Cell,
    collections::HashMap,
    rc::{Rc, Weak},
};

type Builder = dyn Fn(Dialog, &mut Window, &mut App) -> Dialog;

struct Modal {
    build: Box<Builder>,
    open: Cell<bool>,
}

#[derive(Default)]
struct Stacks(HashMap<WindowId, Vec<Weak<Modal>>>);
impl Global for Stacks {}

#[derive(Clone)]
pub(crate) struct Handle(Weak<Modal>);

fn push(modal: Rc<Modal>, window: &mut Window, cx: &mut App) {
    let stack = cx
        .default_global::<Stacks>()
        .0
        .entry(window.window_handle().window_id())
        .or_default();
    stack.retain(|entry| entry.upgrade().is_some_and(|entry| entry.open.get()));
    stack.push(Rc::downgrade(&modal));
    let nested = window.has_active_dialog(cx);
    modal.open.set(true);
    window.refresh();
    window.open_dialog(cx, move |dialog, window, cx| {
        let closed = Rc::downgrade(&modal);
        (modal.build)(dialog.overlay(nested), window, cx).on_close(move |_, window, _| {
            if let Some(closed) = closed.upgrade() {
                closed.open.set(false);
            }
            window.refresh();
        })
    });
}

pub(super) fn open(
    window: &mut Window,
    cx: &mut App,
    build: impl Fn(Dialog, &mut Window, &mut App) -> Dialog + 'static,
) -> Handle {
    let modal = Rc::new(Modal {
        build: Box::new(build),
        open: Cell::new(true),
    });
    let handle = Handle(Rc::downgrade(&modal));
    push(modal, window, cx);
    handle
}

pub(crate) fn close(window: &mut Window, cx: &mut App) {
    let id = window.window_handle().window_id();
    if let Some(stack) = cx.default_global::<Stacks>().0.get_mut(&id)
        && let Some(modal) = stack
            .iter()
            .rev()
            .filter_map(Weak::upgrade)
            .find(|modal| modal.open.get())
    {
        modal.open.set(false);
    }
    window.close_dialog(cx);
}

pub(crate) fn forget_window(id: WindowId, cx: &mut App) {
    if let Some(stack) = cx.default_global::<Stacks>().0.remove(&id) {
        for modal in stack.iter().filter_map(Weak::upgrade) {
            modal.open.set(false);
        }
    }
}

#[cfg(debug_assertions)]
pub(crate) fn close_all(window: &mut Window, cx: &mut App) {
    forget_window(window.window_handle().window_id(), cx);
    window.close_all_dialogs(cx);
}

impl Handle {
    pub(crate) fn is_open(&self) -> bool {
        self.0.upgrade().is_some_and(|modal| modal.open.get())
    }

    pub(crate) fn close(&self, window: &mut Window, cx: &mut App) {
        let Some(target) = self.0.upgrade().filter(|modal| modal.open.get()) else {
            return;
        };
        let id = window.window_handle().window_id();
        let Some(stack) = cx.default_global::<Stacks>().0.get(&id) else {
            return;
        };
        let active: Vec<_> = stack
            .iter()
            .filter_map(Weak::upgrade)
            .filter(|modal| modal.open.get())
            .collect();
        let Some(index) = active.iter().position(|modal| Rc::ptr_eq(modal, &target)) else {
            return;
        };
        let above = &active[index + 1..];
        let focused = if above.is_empty() {
            None
        } else {
            window.focused(cx)
        };
        for _ in &active[index..] {
            close(window, cx);
        }
        for modal in above {
            push(modal.clone(), window, cx);
        }
        if let Some(focus) = focused {
            window.focus(&focus, cx);
        }
    }
}
