//! Main-thread AppKit ownership. Actions cross into GPUI through the foreground receiver.
use super::{
    MenuRequest, Message,
    model::{Action, Icon, Row},
};
use async_channel::Sender;
use objc2::{
    AllocAnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send,
    rc::Retained, runtime::ProtocolObject, sel,
};
use objc2_app_kit::{
    NSBitmapImageRep, NSEventMask, NSImage, NSMenu, NSMenuDelegate, NSMenuItem,
    NSSquareStatusItemLength, NSStatusBar, NSStatusBarButton, NSStatusItem,
};
use objc2_foundation::{NSData, NSObject, NSObjectProtocol, NSSize, NSString};

struct TargetIvars {
    sender: Sender<Message>,
    item: Retained<NSStatusItem>,
    choices: std::cell::RefCell<Vec<Action>>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements. AppKit calls these actions on the main thread.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = TargetIvars]
    struct ShellmateStatusTarget;

    unsafe impl NSObjectProtocol for ShellmateStatusTarget {}
    unsafe impl NSMenuDelegate for ShellmateStatusTarget {
        #[unsafe(method(menuDidClose:))]
        fn menu_did_close(&self, _menu: &NSMenu) {
            self.ivars().item.setMenu(None);
            self.bind_clicks();
        }
    }

    impl ShellmateStatusTarget {
        // Left and right clicks both open the menu; 「打开 Shellmate」 is its first item.
        #[unsafe(method(statusClick:))]
        fn click(&self, _button: &NSStatusBarButton) {
            self.send(Message::Menu(MenuRequest::Pop));
        }

        #[unsafe(method(statusChoice:))]
        fn choose(&self, item: &NSMenuItem) {
            if let Some(&action) = self.ivars().choices.borrow().get(item.tag() as usize) {
                self.send(Message::Run(action));
            }
        }
    }
);

impl ShellmateStatusTarget {
    fn new(
        sender: &Sender<Message>,
        item: Retained<NSStatusItem>,
        mtm: MainThreadMarker,
    ) -> Retained<Self> {
        let target = Self::alloc(mtm).set_ivars(TargetIvars {
            sender: sender.clone(),
            item,
            choices: Default::default(),
        });
        // SAFETY: finish initialization with NSObject's designated initializer.
        unsafe { msg_send![super(target), init] }
    }

    fn bind_clicks(&self) {
        let button = self
            .ivars()
            .item
            .button(self.mtm())
            .expect("AppKit status button");
        // SAFETY: the workspace owns this target and the selector accepts NSStatusBarButton.
        unsafe {
            button.setTarget(Some(self));
            button.setAction(Some(sel!(statusClick:)));
        }
        button.sendActionOn(NSEventMask::LeftMouseUp | NSEventMask::RightMouseUp);
    }

    fn send(&self, message: Message) {
        let _ = self.ivars().sender.try_send(message);
    }

    /// What choosing 「打开 Shellmate」 does, for the debug hook (popping a real menu would block).
    #[cfg(debug_assertions)]
    fn open_shellmate(&self) {
        self.send(Message::Run(Action::Show));
    }
}

fn image(icon: Icon) -> Retained<NSImage> {
    macro_rules! pngs {
        ($name:literal) => {
            [
                include_bytes!(concat!("../../../assets/status-icon/", $name, ".png")).as_slice(),
                include_bytes!(concat!("../../../assets/status-icon/", $name, "@2x.png"))
                    .as_slice(),
            ]
        };
    }
    let pngs = match icon {
        Icon::Idle => pngs!("idle"),
        Icon::Connected(_) => pngs!("connected"),
        Icon::Attention(_) => pngs!("attention"),
    };
    let points = NSSize::new(16., 16.);
    let image = NSImage::initWithSize(NSImage::alloc(), points);
    for bytes in pngs {
        let rep = NSBitmapImageRep::imageRepWithData(&NSData::with_bytes(bytes))
            .expect("embedded status icon PNG");
        rep.setSize(points);
        image.addRepresentation(&rep);
    }
    image.setTemplate(true);
    image
}

pub(super) struct NativeItem {
    button: Retained<NSStatusBarButton>,
    target: Retained<ShellmateStatusTarget>,
}

impl NativeItem {
    pub(super) fn new(sender: &Sender<Message>) -> Self {
        let mtm = MainThreadMarker::new().expect("GPUI foreground thread");
        let item = NSStatusBar::systemStatusBar().statusItemWithLength(NSSquareStatusItemLength);
        let button = item.button(mtm).expect("AppKit status button");
        let target = ShellmateStatusTarget::new(sender, item, mtm);
        target.bind_clicks();
        Self { button, target }
    }

    pub(super) fn update(&self, old: Option<Icon>, icon: Icon) {
        if old.map(Icon::name) != Some(icon.name()) {
            self.button.setImage(Some(&image(icon)));
        }
        let tooltip = icon.tooltip();
        self.button.setToolTip(Some(&NSString::from_str(&tooltip)));
        log::info!("status icon: {} ({tooltip})", icon.name());
    }

    #[cfg(debug_assertions)]
    pub(super) fn debug_click(&self) {
        self.target.open_shellmate();
    }

    pub(super) fn menu(&self, rows: Vec<Row>, request: MenuRequest) {
        let mtm = self.target.mtm();
        let menu = NSMenu::new(mtm);
        menu.setAutoenablesItems(false);
        let mut choices = Vec::new();
        for row in rows {
            let item = match row {
                Row::Separator => NSMenuItem::separatorItem(mtm),
                Row::Item(title, action) => {
                    let item = NSMenuItem::new(mtm);
                    item.setTitle(&NSString::from_str(&title));
                    item.setEnabled(action.is_some());
                    if let Some(action) = action {
                        item.setTag(choices.len() as isize);
                        choices.push(action);
                        // SAFETY: the workspace retains the target and statusChoice: takes NSMenuItem.
                        unsafe {
                            item.setTarget(Some(&self.target));
                            item.setAction(Some(sel!(statusChoice:)));
                        }
                    }
                    item
                }
            };
            menu.addItem(&item);
        }
        match request {
            MenuRequest::Pop => {
                // Keep the snapshot until the next popup. AppKit can deliver the choice after menuDidClose.
                self.target.ivars().choices.replace(choices);
                menu.setDelegate(Some(ProtocolObject::from_ref(&*self.target)));
                self.target.ivars().item.setMenu(Some(&menu));
                // SAFETY: the status button owns the tracking menu and accepts a nil sender.
                unsafe { self.button.performClick(None) };
            }
            #[cfg(debug_assertions)]
            MenuRequest::Dump => {
                for item in menu.itemArray() {
                    let title = if item.isSeparatorItem() {
                        "—".to_owned()
                    } else {
                        item.title().to_string()
                    };
                    log::warn!("status menu: {title} enabled={}", item.isEnabled());
                }
            }
        }
    }
}

impl Drop for NativeItem {
    fn drop(&mut self) {
        NSStatusBar::systemStatusBar().removeStatusItem(&self.target.ivars().item);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_template_has_two_resolutions_at_sixteen_points() {
        for icon in [Icon::Idle, Icon::Connected(1), Icon::Attention(1)] {
            let image = image(icon);
            assert!(image.isTemplate());
            let size = NSSize::new(16., 16.);
            assert_eq!(image.size(), size);
            let mut pixels: Vec<_> = image
                .representations()
                .iter()
                .map(|rep| {
                    assert_eq!(rep.size(), size);
                    (rep.pixelsWide(), rep.pixelsHigh())
                })
                .collect();
            pixels.sort();
            assert_eq!(pixels, [(16, 16), (32, 32)]);
        }
    }
}
