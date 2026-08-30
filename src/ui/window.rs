// SPDX-License-Identifier: GPL-3.0-or-later

use std::{
    cell::{Cell, RefCell},
    env,
    path::PathBuf,
    rc::Rc,
    time::{Duration, Instant},
};

use gtk::{gio, glib, prelude::*};

use crate::{
    adapters::{LocalFileSource, LocalOperationProvider, LocalPreviewProvider},
    app::{Browser, BrowserEvent},
    model::Location,
};

use super::{
    blur::BlurBin,
    browser::{BrowserView, PeekBehavior},
    motion::{animations_enabled, emphasized_deceleration},
    preview::PreviewDrawer,
};

const SIDEBAR_WIDTH: i32 = 208;
const MIN_SIDEBAR_WIDTH: i32 = 176;
const SIDEBAR_TRANSITION: Duration = Duration::from_millis(300);

pub fn present(application: &gtk::Application) {
    present_location(application, None);
}

pub fn present_location(application: &gtk::Application, location: Option<PathBuf>) {
    crate::assets::register_icon_theme();
    let theme_manager = super::theme::ThemeManager::shared();
    load_styles();

    let window = gtk::ApplicationWindow::builder()
        .application(application)
        .title("Strata")
        .default_width(1200)
        .default_height(760)
        .build();

    let browser = BrowserView::new(Rc::new(LocalFileSource), PeekBehavior::default());
    browser.set_operation_provider(Rc::new(LocalOperationProvider));
    let controller = browser.browser();
    let preview = PreviewDrawer::new(Rc::new(LocalPreviewProvider));
    let preview_for_selection = preview.clone();
    let weak_controller = Rc::downgrade(&controller);
    controller.observe(move |event| match event {
        BrowserEvent::PreviewRequested { entry } => preview_for_selection.show(entry),
        BrowserEvent::FocusChanged {
            depth,
            position: Some(position),
        } if preview_for_selection.is_open() => {
            if let Some(entry) = weak_controller
                .upgrade()
                .and_then(|browser| browser.entry_at(depth, position))
            {
                if entry.is_directory() {
                    preview_for_selection.close();
                } else {
                    preview_for_selection.show(entry);
                }
            }
        }
        _ => {}
    });

    let header = gtk::HeaderBar::new();
    header.set_show_title_buttons(false);
    header.set_title_widget(Some(&gtk::Box::new(gtk::Orientation::Horizontal, 0)));
    let sidebar_toggle = gtk::ToggleButton::builder()
        .active(true)
        .tooltip_text("Toggle sidebar")
        .build();
    sidebar_toggle.set_child(Some(&crate::assets::primary_icon(
        crate::assets::icons::PANEL_LEFT,
        20,
    )));
    sidebar_toggle.add_css_class("sidebar-toggle");
    header.pack_start(&sidebar_toggle);
    header.pack_start(&browser.location_widget());
    let search_button = gtk::Button::builder().tooltip_text("Search").build();
    search_button.set_child(Some(&crate::assets::text_icon(
        crate::assets::icons::SEARCH,
        20,
    )));
    search_button.add_css_class("header-action");
    let appearance = build_appearance_menu(&controller);
    let settings = gtk::Button::builder().tooltip_text("Settings").build();
    settings.set_child(Some(&crate::assets::text_icon(
        crate::assets::icons::SETTINGS,
        20,
    )));
    settings.add_css_class("header-action");
    let close_window = gtk::Button::builder().tooltip_text("Close window").build();
    close_window.set_child(Some(&crate::assets::text_icon(crate::assets::icons::X, 20)));
    close_window.add_css_class("header-action");
    let closing_window = window.clone();
    close_window.connect_clicked(move |_| closing_window.close());
    let header_actions = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    header_actions.add_css_class("header-actions");
    header_actions.append(&search_button);
    header_actions.append(&appearance);
    header_actions.append(&settings);
    header_actions.append(&close_window);
    header.pack_end(&header_actions);

    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.append(&header);

    let content = gtk::Paned::new(gtk::Orientation::Horizontal);
    content.set_wide_handle(false);
    content.set_shrink_start_child(false);
    content.set_resize_start_child(false);
    content.set_position(SIDEBAR_WIDTH);
    content.set_vexpand(true);
    let sidebar = build_sidebar(browser.browser(), browser.empty_trash_requester());
    sidebar.widget.set_size_request(MIN_SIDEBAR_WIDTH, -1);
    content.set_start_child(Some(&sidebar.widget));
    content.set_end_child(Some(&browser.widget()));
    let animation_generation = Rc::new(Cell::new(0));
    let sidebar_animating = Rc::new(Cell::new(false));
    let constrained_content = content.clone();
    let constrained_toggle = sidebar_toggle.clone();
    let constrained_animation = sidebar_animating.clone();
    content.connect_position_notify(move |_| {
        if constrained_toggle.is_active()
            && !constrained_animation.get()
            && constrained_content.position() < MIN_SIDEBAR_WIDTH
        {
            constrained_content.set_position(MIN_SIDEBAR_WIDTH);
        }
    });
    let animated_content = content.clone();
    let animated_sidebar = sidebar.widget.clone();
    sidebar_toggle.connect_toggled(move |toggle| {
        animate_sidebar(
            &animated_content,
            &animated_sidebar,
            &animation_generation,
            &sidebar_animating,
            toggle.is_active(),
        );
    });
    let preview_split = gtk::Paned::new(gtk::Orientation::Horizontal);
    preview_split.add_css_class("preview-split");
    preview_split.set_wide_handle(false);
    preview_split.set_resize_start_child(true);
    preview_split.set_resize_end_child(false);
    preview_split.set_shrink_start_child(false);
    preview_split.set_shrink_end_child(true);
    preview_split.set_start_child(Some(&content));
    preview_split.set_end_child(Some(&preview.widget()));
    preview_split.set_position(i32::MAX);
    preview_split.set_vexpand(true);
    let measured_content = content.clone();
    let measured_browser = browser.clone();
    preview.attach_split(
        &preview_split,
        Rc::new(move || measured_content.position() + measured_browser.occupied_width()),
    );
    root.append(&preview_split);

    let window_overlay = gtk::Overlay::new();
    let blurred_root = BlurBin::new(&root);
    window_overlay.set_child(Some(&blurred_root));
    let settings_layer =
        super::settings::build_layer(&browser, &settings, &blurred_root, theme_manager);
    window_overlay.add_overlay(&settings_layer);
    let shown_settings = settings_layer.clone();
    let settings_button = settings.clone();
    let settings_blurred_root = blurred_root.clone();
    settings.connect_clicked(move |_| {
        show_settings(&shown_settings, &settings_button, &settings_blurred_root);
    });
    let settings_shortcut = gtk::EventControllerKey::new();
    let shown_settings = settings_layer.clone();
    let settings_button = settings.clone();
    let shortcut_blurred_root = blurred_root.clone();
    settings_shortcut.connect_key_pressed(move |_, key, _, modifiers| {
        if key != gtk::gdk::Key::comma || !modifiers.contains(gtk::gdk::ModifierType::CONTROL_MASK)
        {
            return glib::Propagation::Proceed;
        }
        show_settings(&shown_settings, &settings_button, &shortcut_blurred_root);
        glib::Propagation::Stop
    });
    window.add_controller(settings_shortcut);
    window.set_child(Some(&window_overlay));
    install_modal_focus_trap(&window);
    install_keyboard_navigation(&window, &browser, &sidebar_toggle, &preview);
    browser.navigate(location.unwrap_or_else(home_directory));

    let browser_controller = browser.browser();
    window.connect_destroy(move |_| {
        browser_controller.clear_observer();
        sidebar.disconnect();
    });
    window.present();
    crate::metrics::mark_window_presented();
}

fn animate_sidebar(
    paned: &gtk::Paned,
    sidebar: &gtk::Widget,
    generation: &Rc<Cell<u64>>,
    animating: &Rc<Cell<bool>>,
    expanded: bool,
) {
    let animation_id = generation.get().saturating_add(1);
    generation.set(animation_id);
    animating.set(true);
    paned.set_shrink_start_child(true);
    let target = if expanded { SIDEBAR_WIDTH } else { 0 };
    let start = paned.position();
    if expanded {
        sidebar.set_visible(true);
    }

    if !animations_enabled() || start == target {
        paned.set_position(target);
        sidebar.set_visible(expanded);
        paned.set_shrink_start_child(!expanded);
        animating.set(false);
        return;
    }

    let started = Instant::now();
    let paned = paned.clone();
    let sidebar = sidebar.clone();
    let generation = generation.clone();
    let animating = animating.clone();
    let _tick = paned.clone().add_tick_callback(move |_, _| {
        if generation.get() != animation_id {
            return glib::ControlFlow::Break;
        }

        let progress =
            (started.elapsed().as_secs_f64() / SIDEBAR_TRANSITION.as_secs_f64()).clamp(0.0, 1.0);
        let eased = emphasized_deceleration(progress);
        let position = f64::from(start) + f64::from(target - start) * eased;
        paned.set_position(position.round() as i32);

        if progress >= 1.0 {
            paned.set_position(target);
            if !expanded {
                sidebar.set_visible(false);
            }
            paned.set_shrink_start_child(!expanded);
            animating.set(false);
            glib::ControlFlow::Break
        } else {
            glib::ControlFlow::Continue
        }
    });
}

fn install_keyboard_navigation(
    window: &gtk::ApplicationWindow,
    view: &BrowserView,
    sidebar_toggle: &gtk::ToggleButton,
    preview: &PreviewDrawer,
) {
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    let view = view.clone();
    let sidebar_toggle = sidebar_toggle.clone();
    let preview = preview.clone();
    let dialog_parent = window.clone();
    let weak_browser = Rc::downgrade(&view.browser());
    keys.connect_key_pressed(move |_, key, _, modifiers| {
        let Some(browser) = weak_browser.upgrade() else {
            return glib::Propagation::Proceed;
        };
        if let Some(layer) = visible_modal_layer(&dialog_parent) {
            let focus_is_inside = gtk::prelude::RootExt::focus(&dialog_parent)
                .is_some_and(|focus| focus == layer || focus.is_ancestor(&layer));
            if !focus_is_inside {
                layer.grab_focus();
                return glib::Propagation::Stop;
            }
            return glib::Propagation::Proceed;
        }
        let alt = modifiers.contains(gtk::gdk::ModifierType::ALT_MASK);
        let control = modifiers.contains(gtk::gdk::ModifierType::CONTROL_MASK);
        let shift = modifiers.contains(gtk::gdk::ModifierType::SHIFT_MASK);
        if key == gtk::gdk::Key::F2 && view.begin_rename() {
            return glib::Propagation::Stop;
        }
        if key == gtk::gdk::Key::Escape && view.cancel_new_folder() {
            return glib::Propagation::Stop;
        }
        if key == gtk::gdk::Key::Escape && view.cancel_rename() {
            return glib::Propagation::Stop;
        }
        if view.rename_is_active() || view.new_folder_is_active() {
            return glib::Propagation::Proceed;
        }
        if key == gtk::gdk::Key::Escape && view.dismiss_focused_filter() {
            return glib::Propagation::Stop;
        }
        if control && key == gtk::gdk::Key::l {
            view.begin_location_edit();
            return glib::Propagation::Stop;
        }
        if control && key == gtk::gdk::Key::b {
            sidebar_toggle.set_active(!sidebar_toggle.is_active());
            return glib::Propagation::Stop;
        }
        if view.location_has_focus() {
            if key == gtk::gdk::Key::Escape {
                view.cancel_location_edit();
                return glib::Propagation::Stop;
            }
            return glib::Propagation::Proceed;
        }
        if control && shift && matches!(key, gtk::gdk::Key::n | gtk::gdk::Key::N) {
            view.create_new_folder();
            return glib::Propagation::Stop;
        }
        if control && !shift && key == gtk::gdk::Key::v {
            if view.filter_has_focus() {
                return glib::Propagation::Proceed;
            }
            view.paste();
            return glib::Propagation::Stop;
        }
        if control && !shift && key == gtk::gdk::Key::a {
            if view.filter_has_focus() {
                return glib::Propagation::Proceed;
            }
            view.select_all();
            return glib::Propagation::Stop;
        }
        if control && key == gtk::gdk::Key::h {
            browser.toggle_hidden();
            return glib::Propagation::Stop;
        }
        if key == gtk::gdk::Key::Delete && !view.filter_has_focus() && view.confirm_delete(shift) {
            return glib::Propagation::Stop;
        }
        if key == gtk::gdk::Key::space && !alt && !control {
            preview.toggle(browser.focused_entry());
            return glib::Propagation::Stop;
        }
        if key == gtk::gdk::Key::Escape && preview.is_open() {
            preview.close();
            return glib::Propagation::Stop;
        }
        if shift && key == gtk::gdk::Key::Up {
            browser.extend_selection(-1);
            return glib::Propagation::Stop;
        }
        if shift && key == gtk::gdk::Key::Down {
            browser.extend_selection(1);
            return glib::Propagation::Stop;
        }

        match (key, alt) {
            (gtk::gdk::Key::Left, true) => browser.back(),
            (gtk::gdk::Key::Right, true) => browser.forward(),
            (gtk::gdk::Key::Up, true) => browser.parent(),
            (gtk::gdk::Key::Home, true) => {
                browser.navigate(Location::local(home_directory()));
            }
            (gtk::gdk::Key::j | gtk::gdk::Key::Down, false) => browser.move_selection(1),
            (gtk::gdk::Key::k | gtk::gdk::Key::Up, false) => browser.move_selection(-1),
            (gtk::gdk::Key::h | gtk::gdk::Key::Left, false) => browser.focus_parent(),
            (
                gtk::gdk::Key::l
                | gtk::gdk::Key::Right
                | gtk::gdk::Key::Return
                | gtk::gdk::Key::KP_Enter,
                false,
            ) => browser.activate_focused(),
            (gtk::gdk::Key::Escape, false) => browser.escape(),
            _ => return glib::Propagation::Proceed,
        }
        glib::Propagation::Stop
    });
    window.add_controller(keys);
}

fn visible_modal_layer(window: &gtk::ApplicationWindow) -> Option<gtk::Widget> {
    let overlay = window.child().and_downcast::<gtk::Overlay>()?;
    let mut child = overlay.first_child();
    while let Some(widget) = child {
        if widget.is_visible() && widget.has_css_class("app-modal-layer") {
            return Some(widget);
        }
        child = widget.next_sibling();
    }
    None
}

fn install_modal_focus_trap(window: &gtk::ApplicationWindow) {
    window.connect_focus_widget_notify(|window| {
        let Some(layer) = visible_modal_layer(window) else {
            return;
        };
        let focus_is_inside = gtk::prelude::RootExt::focus(window)
            .is_some_and(|focus| focus == layer || focus.is_ancestor(&layer));
        if !focus_is_inside {
            layer.grab_focus();
        }
    });
}

fn build_appearance_menu(controller: &Rc<Browser>) -> gtk::MenuButton {
    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.add_css_class("appearance-menu");
    append_menu_heading(&content, "VIEW");
    let (list, _, _) = appearance_option(crate::assets::icons::LIST, "List", true, true);
    content.append(&list);
    let (grid, _, _) = appearance_option(crate::assets::icons::GRID, "Grid", false, false);
    grid.set_tooltip_text(Some("Grid view is planned"));
    content.append(&grid);

    content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    append_menu_heading(&content, "DENSITY");
    let (compact, _, _) = appearance_option(crate::assets::icons::ROWS, "Compact", true, true);
    content.append(&compact);
    let (airy, _, _) = appearance_option(crate::assets::icons::ROWS, "Airy", false, false);
    airy.set_tooltip_text(Some("Airy density is planned"));
    content.append(&airy);

    content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    let (hidden, hidden_check, hidden_icon) =
        appearance_option(crate::assets::icons::EYE_OFF, "Hidden files", false, true);
    let hidden_state = Rc::new(Cell::new(false));
    let weak_controller = Rc::downgrade(controller);
    hidden.connect_clicked(move |_| {
        let shown = !hidden_state.get();
        hidden_state.set(shown);
        hidden_check.set_visible(shown);
        crate::assets::set_primary_icon(
            &hidden_icon,
            if shown {
                crate::assets::icons::EYE
            } else {
                crate::assets::icons::EYE_OFF
            },
        );
        if let Some(controller) = weak_controller.upgrade() {
            controller.toggle_hidden();
        }
    });
    content.append(&hidden);

    let popover = gtk::Popover::builder()
        .child(&content)
        .has_arrow(false)
        .halign(gtk::Align::End)
        .position(gtk::PositionType::Bottom)
        .build();
    popover.add_css_class("appearance-popover");
    let button = gtk::MenuButton::builder()
        .tooltip_text("Appearance")
        .popover(&popover)
        .build();
    let icon = crate::assets::text_icon(crate::assets::icons::LIST, 20);
    button.set_child(Some(&icon));
    button.add_css_class("header-action");
    button.connect_active_notify(move |button| {
        crate::assets::set_text_icon(
            &icon,
            if button.is_active() {
                crate::assets::icons::LIST_ACTIVE
            } else {
                crate::assets::icons::LIST
            },
        );
    });
    button
}

fn appearance_option(
    icon: &str,
    label: &str,
    checked: bool,
    sensitive: bool,
) -> (gtk::Button, gtk::Image, gtk::Image) {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    let check = crate::assets::primary_icon(crate::assets::icons::CHECK, 16);
    check.set_visible(checked);
    let option = crate::assets::primary_icon(icon, 17);
    let label = gtk::Label::new(Some(label));
    label.set_xalign(0.0);
    label.set_hexpand(true);
    row.append(&option);
    row.append(&label);
    row.append(&check);
    let button = gtk::Button::builder()
        .child(&row)
        .sensitive(sensitive)
        .build();
    button.add_css_class("appearance-option");
    button.set_has_frame(false);
    (button, check, option)
}

fn show_settings(layer: &gtk::Box, button: &gtk::Button, root: &BlurBin) {
    root.set_blurred(true);
    layer.set_visible(true);
    layer.grab_focus();
    button.add_css_class("active");
}

fn append_menu_heading(container: &gtk::Box, text: &str) {
    let heading = gtk::Label::new(Some(text));
    heading.set_xalign(0.0);
    heading.add_css_class("menu-heading");
    container.append(&heading);
}

struct SidebarState {
    widget: gtk::Box,
    browser: Rc<Browser>,
    volume_monitor: gio::VolumeMonitor,
    place_order: RefCell<Vec<&'static str>>,
    place_rows: RefCell<Vec<(Location, gtk::Button)>>,
    empty_trash: Rc<dyn Fn()>,
}

struct SidebarView {
    widget: gtk::Widget,
    state: Rc<SidebarState>,
    handlers: RefCell<Vec<glib::SignalHandlerId>>,
}

impl SidebarView {
    fn disconnect(&self) {
        for handler in self.handlers.take() {
            self.state.volume_monitor.disconnect(handler);
        }
    }
}

impl SidebarState {
    fn rebuild(self: &Rc<Self>) {
        while let Some(child) = self.widget.first_child() {
            self.widget.remove(&child);
        }
        self.place_rows.borrow_mut().clear();

        self.append_place(
            crate::assets::icons::HOME,
            "Home",
            Location::local(home_directory()),
        );
        self.append_trash_place();
        self.append_separator();

        for place in self.place_order.borrow().clone() {
            if let Some((icon, name, directory)) = standard_place(place) {
                if let Some(path) = glib::user_special_dir(directory)
                    .filter(|path| should_show_standard_place(place, path, &home_directory()))
                {
                    self.append_reorderable_place(place, icon, name, Location::local(path));
                }
            }
        }

        let volumes = self.volume_monitor.volumes();
        let mounts: Vec<_> = self
            .volume_monitor
            .mounts()
            .into_iter()
            .filter(|mount| !mount.is_shadowed() && mount.volume().is_none())
            .map(|mount| {
                let root = mount.root();
                let location = root
                    .path()
                    .map(Location::local)
                    .unwrap_or_else(|| Location::uri(root.uri()));
                (mount.name().to_string(), location)
            })
            .collect();
        if !volumes.is_empty() || !mounts.is_empty() {
            self.append_separator();
            for volume in volumes {
                self.append_volume(volume);
            }
            for (name, location) in mounts {
                self.append_place(crate::assets::icons::HARD_DRIVE, &name, location);
            }
        }
        self.sync_active_place();
    }

    fn sync_active_place(&self) {
        let active = self.browser.active_location();
        let rows = self.place_rows.borrow();
        let selected = rows
            .iter()
            .position(|(location, row)| {
                active.as_ref() == Some(location) && row.has_css_class("active")
            })
            .or_else(|| {
                rows.iter()
                    .position(|(location, _)| active.as_ref() == Some(location))
            });
        for (index, (_, row)) in rows.iter().enumerate() {
            if selected == Some(index) {
                row.add_css_class("active");
            } else {
                row.remove_css_class("active");
            }
        }
    }

    fn append_reorderable_place(
        self: &Rc<Self>,
        id: &'static str,
        icon: &str,
        name: &str,
        location: Location,
    ) {
        let row = sidebar_button(icon, name);
        row.add_css_class("reorderable");
        row.set_cursor_from_name(Some("grab"));
        row.set_tooltip_text(Some(&location.display_path()));
        self.place_rows
            .borrow_mut()
            .push((location.clone(), row.clone()));
        let weak_browser = Rc::downgrade(&self.browser);
        let sidebar = self.widget.clone();
        let selected_row = row.clone();
        row.connect_clicked(move |_| {
            select_sidebar_row(&sidebar, &selected_row);
            if let Some(browser) = weak_browser.upgrade() {
                browser.navigate(location.clone());
            }
        });

        let drag = gtk::DragSource::builder()
            .actions(gtk::gdk::DragAction::MOVE)
            .build();
        drag.connect_prepare(move |_, _, _| {
            Some(gtk::gdk::ContentProvider::for_value(
                &id.to_string().to_value(),
            ))
        });
        let dragged_row = row.clone();
        drag.connect_drag_begin(move |_, _| {
            dragged_row.add_css_class("dragging");
            dragged_row.set_cursor_from_name(Some("grabbing"));
        });
        let dragged_row = row.clone();
        drag.connect_drag_end(move |_, _, _| {
            dragged_row.remove_css_class("dragging");
            dragged_row.set_cursor_from_name(Some("grab"));
        });
        row.add_controller(drag);

        let drop = gtk::DropTarget::new(String::static_type(), gtk::gdk::DragAction::MOVE);
        let weak_state = Rc::downgrade(self);
        let target_row = row.clone();
        drop.connect_drop(move |_, value, _, y| {
            let Ok(source) = value.get::<String>() else {
                return false;
            };
            let after = y >= f64::from(target_row.height()) / 2.0;
            if let Some(state) = weak_state.upgrade() {
                state.reorder_place(&source, id, after);
                return true;
            }
            false
        });
        row.add_controller(drop);
        self.widget.append(&row);
    }

    fn reorder_place(self: &Rc<Self>, source: &str, target: &str, after: bool) {
        let changed = reorder_places(&mut self.place_order.borrow_mut(), source, target, after);
        if changed {
            self.rebuild();
        }
    }

    fn append_separator(&self) {
        let separator = gtk::Separator::new(gtk::Orientation::Horizontal);
        separator.add_css_class("sidebar-separator");
        self.widget.append(&separator);
    }

    fn append_volume(&self, volume: gio::Volume) {
        let name = volume.name().to_string();
        let row = sidebar_button(crate::assets::icons::HARD_DRIVE, &name);
        row.set_tooltip_text(Some(&name));
        if let Some(mount) = volume.get_mount() {
            let root = mount.root();
            let location = root
                .path()
                .map(Location::local)
                .unwrap_or_else(|| Location::uri(root.uri()));
            self.place_rows.borrow_mut().push((location, row.clone()));
        }
        let weak_browser = Rc::downgrade(&self.browser);
        let sidebar = self.widget.clone();
        let selected_row = row.clone();
        row.connect_clicked(move |button| {
            select_sidebar_row(&sidebar, &selected_row);
            let Some(browser) = weak_browser.upgrade() else {
                return;
            };
            if let Some(mount) = volume.get_mount() {
                navigate_to_gio_file(&browser, &mount.root());
                return;
            }

            let window = button.root().and_downcast::<gtk::Window>();
            let operation = gtk::MountOperation::new(window.as_ref());
            let volume = volume.clone();
            glib::MainContext::default().spawn_local(async move {
                match volume
                    .mount_future(gio::MountMountFlags::NONE, Some(&operation))
                    .await
                {
                    Ok(()) => {
                        if let Some(mount) = volume.get_mount() {
                            navigate_to_gio_file(&browser, &mount.root());
                        }
                    }
                    Err(error) => {
                        let dialog = gtk::AlertDialog::builder()
                            .modal(true)
                            .message("Unable to mount volume")
                            .detail(error.to_string())
                            .build();
                        dialog.show(window.as_ref());
                    }
                }
            });
        });
        self.widget.append(&row);
    }

    fn append_trash_place(&self) {
        let location = Location::uri("trash:///");
        let row = sidebar_button(crate::assets::icons::TRASH, "Trash");
        row.set_hexpand(true);
        row.set_tooltip_text(Some(&location.display_path()));
        self.place_rows
            .borrow_mut()
            .push((location.clone(), row.clone()));
        let weak_browser = Rc::downgrade(&self.browser);
        let sidebar = self.widget.clone();
        let selected_row = row.clone();
        row.connect_clicked(move |_| {
            select_sidebar_row(&sidebar, &selected_row);
            if let Some(browser) = weak_browser.upgrade() {
                browser.navigate(location.clone());
            }
        });

        let empty = gtk::Button::builder().tooltip_text("Empty Trash…").build();
        empty.set_child(Some(&crate::assets::danger_icon(
            crate::assets::icons::X,
            14,
        )));
        empty.add_css_class("sidebar-row-action");
        empty.set_has_frame(false);
        empty.set_valign(gtk::Align::Center);
        let empty_trash = self.empty_trash.clone();
        empty.connect_clicked(move |_| empty_trash());

        let container = gtk::Box::new(gtk::Orientation::Horizontal, 2);
        container.add_css_class("sidebar-trash-row");
        container.append(&row);
        container.append(&empty);
        self.widget.append(&container);
    }

    fn append_place(&self, icon: &str, name: &str, location: Location) {
        let row = sidebar_button(icon, name);
        row.set_tooltip_text(Some(&location.display_path()));
        self.place_rows
            .borrow_mut()
            .push((location.clone(), row.clone()));
        let weak_browser = Rc::downgrade(&self.browser);
        let sidebar = self.widget.clone();
        let selected_row = row.clone();
        row.connect_clicked(move |_| {
            select_sidebar_row(&sidebar, &selected_row);
            if let Some(browser) = weak_browser.upgrade() {
                browser.navigate(location.clone());
            }
        });
        self.widget.append(&row);
    }
}

fn select_sidebar_row(sidebar: &gtk::Box, selected: &gtk::Button) {
    let mut child = sidebar.first_child();
    while let Some(widget) = child {
        if let Ok(row) = widget.clone().downcast::<gtk::Button>() {
            row.remove_css_class("active");
        } else {
            let mut inner = widget.first_child();
            while let Some(inner_widget) = inner {
                if let Ok(row) = inner_widget.clone().downcast::<gtk::Button>() {
                    row.remove_css_class("active");
                }
                inner = inner_widget.next_sibling();
            }
        }
        child = widget.next_sibling();
    }
    selected.add_css_class("active");
}

fn reorder_places(order: &mut Vec<&'static str>, source: &str, target: &str, after: bool) -> bool {
    if source == target {
        return false;
    }
    let Some(source_index) = order.iter().position(|place| *place == source) else {
        return false;
    };
    let source = order.remove(source_index);
    let Some(target_index) = order.iter().position(|place| *place == target) else {
        order.insert(source_index, source);
        return false;
    };
    order.insert(target_index + usize::from(after), source);
    true
}

fn should_show_standard_place(id: &str, path: &std::path::Path, home: &std::path::Path) -> bool {
    id != "desktop" || path != home
}

fn standard_place(id: &str) -> Option<(&'static str, &'static str, glib::UserDirectory)> {
    match id {
        "desktop" => Some((
            crate::assets::icons::FOLDER,
            "Desktop",
            glib::UserDirectory::Desktop,
        )),
        "documents" => Some((
            crate::assets::icons::DOCUMENTS,
            "Documents",
            glib::UserDirectory::Documents,
        )),
        "downloads" => Some((
            crate::assets::icons::DOWNLOADS,
            "Downloads",
            glib::UserDirectory::Downloads,
        )),
        "pictures" => Some((
            crate::assets::icons::PICTURES,
            "Pictures",
            glib::UserDirectory::Pictures,
        )),
        "videos" => Some((
            crate::assets::icons::VIDEOS,
            "Videos",
            glib::UserDirectory::Videos,
        )),
        _ => None,
    }
}

fn sidebar_button(icon: &str, name: &str) -> gtk::Button {
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let image = crate::assets::primary_icon(icon, 17);
    let label = gtk::Label::new(Some(name));
    label.set_xalign(0.0);
    label.set_hexpand(true);
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    content.append(&image);
    content.append(&label);

    let row = gtk::Button::builder()
        .child(&content)
        .halign(gtk::Align::Fill)
        .build();
    row.add_css_class("sidebar-row");
    row.set_has_frame(false);
    row
}

fn navigate_to_gio_file(browser: &Rc<Browser>, file: &gio::File) {
    let location = file
        .path()
        .map(Location::local)
        .unwrap_or_else(|| Location::uri(file.uri()));
    browser.navigate(location);
}

fn build_sidebar(browser: Rc<Browser>, empty_trash: Rc<dyn Fn()>) -> SidebarView {
    let widget = gtk::Box::new(gtk::Orientation::Vertical, 2);
    widget.add_css_class("sidebar");
    let scroller = gtk::ScrolledWindow::builder()
        .child(&widget)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .width_request(SIDEBAR_WIDTH)
        .build();
    scroller.add_css_class("sidebar-scroll");
    let volume_monitor = gio::VolumeMonitor::get();
    let state = Rc::new(SidebarState {
        widget,
        browser,
        volume_monitor,
        place_order: RefCell::new(vec![
            "desktop",
            "documents",
            "downloads",
            "pictures",
            "videos",
        ]),
        place_rows: RefCell::new(Vec::new()),
        empty_trash,
    });

    let weak = Rc::downgrade(&state);
    state.browser.observe(move |_| {
        if let Some(state) = weak.upgrade() {
            state.sync_active_place();
        }
    });

    let mut handlers = Vec::new();
    let weak = Rc::downgrade(&state);
    handlers.push(state.volume_monitor.connect_mount_added(move |_, _| {
        if let Some(state) = weak.upgrade() {
            state.rebuild();
        }
    }));
    let weak = Rc::downgrade(&state);
    handlers.push(state.volume_monitor.connect_mount_removed(move |_, _| {
        if let Some(state) = weak.upgrade() {
            state.rebuild();
        }
    }));
    let weak = Rc::downgrade(&state);
    handlers.push(state.volume_monitor.connect_mount_changed(move |_, _| {
        if let Some(state) = weak.upgrade() {
            state.rebuild();
        }
    }));
    let weak = Rc::downgrade(&state);
    handlers.push(state.volume_monitor.connect_volume_added(move |_, _| {
        if let Some(state) = weak.upgrade() {
            state.rebuild();
        }
    }));
    let weak = Rc::downgrade(&state);
    handlers.push(state.volume_monitor.connect_volume_removed(move |_, _| {
        if let Some(state) = weak.upgrade() {
            state.rebuild();
        }
    }));
    let weak = Rc::downgrade(&state);
    handlers.push(state.volume_monitor.connect_volume_changed(move |_, _| {
        if let Some(state) = weak.upgrade() {
            state.rebuild();
        }
    }));
    state.rebuild();

    SidebarView {
        widget: scroller.upcast(),
        state,
        handlers: RefCell::new(handlers),
    }
}

fn home_directory() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

#[cfg(test)]
mod tests;

fn load_styles() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(include_str!("../style.css"));

    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}
