use std::{fmt::Write as _, path::PathBuf};
use gpui::{Action, App, AsKeystroke as _, KeyContext, Keymap, OperatingSystem, OwnedMenu, OwnedMenuItem, Window};

pub(super) fn shortcut(keymap: &Keymap, action: &dyn Action) -> String {
    let mut context = KeyContext::new_with_defaults(OperatingSystem::Mac);
    for name in ["Workspace", "Pane", "Editor"] { context.add(name); }
    let mut bindings = keymap.bindings_for_action(action);
    let first = bindings.next();
    let binding = first.into_iter().chain(bindings).find(|binding| {
        binding.predicate().is_none_or(|predicate| predicate.eval(std::slice::from_ref(&context)))
    }).or(first);
    match binding.map(|binding| binding.keystrokes()) {
        Some([key]) => gpui::component::kbd::Kbd::format(key.as_keystroke()),
        _ => "—".into(),
    }
}

pub(super) fn audit_keymap(keymap: &Keymap) -> Result<(), String> {
    use std::collections::HashSet;
    use gpui::{Keystroke, component::input};
    use shellmate_term::view as terminal;
    let mut globals = HashSet::new();
    for binding in keymap.bindings().filter(|binding| binding.predicate().is_none()) {
        if !globals.insert(binding.keystrokes()) { return Err(format!("duplicate global shortcut: {:?}", binding.keystrokes())); }
    }
    let edits: [(&str, &dyn Action, &dyn Action); 6] = [
        ("cmd-z", &input::Undo, &super::EditUndo), ("cmd-shift-z", &input::Redo, &super::EditRedo),
        ("cmd-x", &input::Cut, &super::EditCut), ("cmd-c", &input::Copy, &terminal::Copy),
        ("cmd-v", &input::Paste, &terminal::Paste), ("cmd-a", &input::SelectAll, &terminal::SelectAll),
    ];
    for path in ["Workspace/Terminal", "Workspace/Input", "Workspace/Terminal/TerminalSearch/Input", "Editor/Input mode=editor"] {
        let contexts = path.split('/').map(|name| KeyContext::parse(name).unwrap()).collect::<Vec<_>>();
        for (key, input, terminal) in edits {
            let expected = if path.ends_with("Terminal") { terminal } else { input };
            let (matches, _) = keymap.bindings_for_input(&[Keystroke::parse(key).unwrap()], &contexts);
            if matches.first().is_none_or(|binding| !binding.action().partial_eq(expected)) {
                return Err(format!("{path} {key}: expected {}, got {:?}", expected.name(), matches.first().map(|b| b.action().name())));
            }
        }
    }
    Ok(())
}

fn rows(menu: OwnedMenu, cx: &mut App, output: &mut String) {
    let keymap = cx.key_bindings();
    for item in menu.items {
        let (label, shortcut, checked, disabled, available) = match item {
            OwnedMenuItem::Action { name, action, checked, disabled, .. } => {
                let available = cx.is_action_available(action.as_ref());
                (name, shortcut(&keymap.borrow(), action.as_ref()), checked, disabled || !available, available.to_string())
            }
            OwnedMenuItem::SystemMenu(system) => (system.name.to_string(), "—".into(), false, false, "n/a".into()),
            OwnedMenuItem::Separator => ("—".into(), "—".into(), false, false, "n/a".into()),
            OwnedMenuItem::Submenu(submenu) => { rows(submenu, cx, output); continue; }
        };
        writeln!(output, "{}\t{label}\t{shortcut}\t{checked}\t{disabled}\t{available}", menu.name).unwrap();
    }
}

pub(crate) fn after_frame(path: PathBuf, window: &mut Window) {
    window.on_next_frame(move |window, _| {
        window.on_next_frame(move |_, cx| cx.defer(move |cx| {
            let mut output = String::from("menu\tlabel\tshortcut\tchecked\tdisabled\tavailable\n");
            if let Err(error) = audit_keymap(&cx.key_bindings().borrow()) { log::error!("menu keymap audit: {error}"); }
            for menu in cx.get_menus().unwrap_or_default() { rows(menu, cx, &mut output); }
            if let Err(error) = std::fs::write(&path, output) { log::error!("menu dump {}: {error}", path.display()); }
        }));
        window.refresh();
    });
    window.refresh();
}
