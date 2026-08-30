// SPDX-License-Identifier: GPL-3.0-or-later

use std::{
    cell::{Cell, RefCell},
    path::Path,
    rc::Rc,
    time::{Duration, Instant},
};

use gtk::{gio, glib, prelude::*};

use crate::{
    app::{Browser, BrowserEvent},
    model::{FileEntry, Location, SortDirection, SortKey},
    services::{FileSource, OperationProvider},
};

use super::{
    blur::BlurBin,
    motion::{animations_enabled, emphasized_deceleration},
};

const COLUMN_WIDTH: i32 = 300;
const COLUMN_OFFSET: i32 = 24;
const COLUMN_TRANSITION: Duration = Duration::from_millis(220);

#[derive(Clone)]
struct LoadPresentation {
    stack: gtk::Stack,
    skeleton: gtk::Box,
    feedback: gtk::Box,
    message: gtk::Label,
    retry: Option<gtk::Button>,
}

struct BoundRow {
    item: glib::WeakRef<gtk::ListItem>,
    row: glib::WeakRef<gtk::Box>,
}

#[derive(Clone)]
struct ColumnView {
    shell: gtk::Box,
    animation_generation: Rc<Cell<u64>>,
    presentation: LoadPresentation,
    model: gtk::StringList,
    filtered_model: gtk::FilterListModel,
    filter_entry: gtk::Entry,
    filter_button: gtk::ToggleButton,
    selection: gtk::MultiSelection,
    syncing_selection: Rc<Cell<bool>>,
    list: gtk::ListView,
    marquee: gtk::Box,
    bound_rows: Rc<RefCell<Vec<BoundRow>>>,
    entry_count: Rc<Cell<usize>>,
    spinner: gtk::Spinner,
    new_folder_row: gtk::Box,
    new_folder_entry: gtk::Entry,
}

struct ActiveRename {
    entry: FileEntry,
    field: gtk::Entry,
    label: gtk::Label,
    spacer: gtk::Box,
}

struct ActiveNewFolder {
    location: Location,
    row: gtk::Box,
    field: gtk::Entry,
}

struct PeekView {
    revealer: gtk::Revealer,
    location: Location,
    presentation: LoadPresentation,
    model: gtk::StringList,
    entry_count: Rc<Cell<usize>>,
    spinner: gtk::Spinner,
}

impl LoadPresentation {
    fn new(content: &impl IsA<gtk::Widget>, retry: Option<gtk::Button>) -> Self {
        let skeleton = gtk::Box::new(gtk::Orientation::Vertical, 9);
        skeleton.add_css_class("loading-skeleton");
        for width in [168, 124, 192, 148, 176, 112] {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            row.add_css_class("skeleton-row");
            row.set_size_request(width, 10);
            row.set_halign(gtk::Align::Start);
            skeleton.append(&row);
        }

        let feedback = gtk::Box::new(gtk::Orientation::Vertical, 8);
        feedback.add_css_class("directory-feedback");
        feedback.set_halign(gtk::Align::Center);
        feedback.set_valign(gtk::Align::Center);
        let message = gtk::Label::new(None);
        message.add_css_class("status-message");
        message.set_justify(gtk::Justification::Center);
        message.set_wrap(true);
        feedback.append(&message);
        if let Some(button) = retry.as_ref() {
            button.set_halign(gtk::Align::Center);
            feedback.append(button);
        }

        let stack = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::Crossfade)
            .transition_duration(100)
            .hexpand(true)
            .vexpand(true)
            .build();
        stack.add_named(content, Some("content"));
        stack.add_named(&skeleton, Some("loading"));
        stack.add_named(&feedback, Some("feedback"));
        stack.set_visible_child_name("loading");

        Self {
            stack,
            skeleton,
            feedback,
            message,
            retry,
        }
    }

    fn show_loading(&self) {
        self.skeleton.set_visible(true);
        self.feedback.set_visible(true);
        if let Some(retry) = self.retry.as_ref() {
            retry.set_visible(false);
        }
        self.stack.set_visible_child_name("loading");
    }

    fn show_content(&self) {
        self.stack.set_visible_child_name("content");
    }

    fn show_empty(&self) {
        self.message.set_text("This directory is empty");
        self.message.remove_css_class("error");
        if let Some(retry) = self.retry.as_ref() {
            retry.set_visible(false);
        }
        self.stack.set_visible_child_name("feedback");
    }

    fn show_error(&self, message: &str) {
        self.message.set_text(message);
        self.message.add_css_class("error");
        if let Some(retry) = self.retry.as_ref() {
            retry.set_visible(true);
        }
        self.stack.set_visible_child_name("feedback");
    }
}

#[derive(Clone, Copy)]
pub struct PeekBehavior {
    pub open_delay: Duration,
    pub close_delay: Duration,
    pub fade_duration: Duration,
    pub item_limit: usize,
}

impl Default for PeekBehavior {
    fn default() -> Self {
        Self {
            open_delay: Duration::from_millis(180),
            close_delay: Duration::from_millis(80),
            fade_duration: Duration::from_millis(100),
            item_limit: 8,
        }
    }
}

struct ViewState {
    overlay: gtk::Overlay,
    location_stack: gtk::Stack,
    breadcrumbs: gtk::Box,
    location_entry: gtk::Entry,
    location_error: gtk::Label,
    columns_widget: gtk::Box,
    scroller: gtk::ScrolledWindow,
    columns: RefCell<Vec<ColumnView>>,
    horizontal_scroll_generation: Rc<Cell<u64>>,
    peek: RefCell<Option<PeekView>>,
    pending_peek: RefCell<Option<glib::SourceId>>,
    pending_close: RefCell<Option<glib::SourceId>>,
    peek_anchor: RefCell<Option<gtk::Widget>>,
    peek_behavior: PeekBehavior,
    peek_enabled: Cell<bool>,
    active_rename: RefCell<Option<ActiveRename>>,
    active_new_folder: RefCell<Option<ActiveNewFolder>>,
    browser: Rc<Browser>,
}

#[derive(Clone)]
pub struct BrowserView {
    state: Rc<ViewState>,
}

impl BrowserView {
    pub fn new(source: Rc<dyn FileSource>, peek_behavior: PeekBehavior) -> Self {
        let columns_widget = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        columns_widget.add_css_class("columns");
        columns_widget.set_halign(gtk::Align::Start);
        columns_widget.set_vexpand(true);

        let scroller = gtk::ScrolledWindow::builder()
            .child(&columns_widget)
            .hscrollbar_policy(gtk::PolicyType::Automatic)
            .vscrollbar_policy(gtk::PolicyType::Never)
            .hexpand(true)
            .vexpand(true)
            .build();
        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&scroller));

        let location_entry = gtk::Entry::builder()
            .hexpand(true)
            .width_chars(48)
            .placeholder_text("Enter an absolute path")
            .tooltip_text("Location (Ctrl+L)")
            .build();
        location_entry.add_css_class("location-entry");
        let location_error = gtk::Label::new(None);
        location_error.add_css_class("location-error");
        location_error.set_visible(false);
        location_error.set_xalign(0.0);
        let confirm_location = gtk::Button::builder()
            .tooltip_text("Navigate (Enter)")
            .build();
        confirm_location.set_child(Some(&crate::assets::primary_icon(
            crate::assets::icons::CHECK,
            16,
        )));
        confirm_location.add_css_class("location-action");
        let cancel_location = gtk::Button::builder()
            .tooltip_text("Cancel (Escape)")
            .build();
        cancel_location.set_child(Some(&crate::assets::primary_icon(
            crate::assets::icons::X,
            16,
        )));
        cancel_location.add_css_class("location-action");
        let entry_row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        entry_row.append(&location_entry);
        entry_row.append(&confirm_location);
        entry_row.append(&cancel_location);
        let entry_control = gtk::Box::new(gtk::Orientation::Vertical, 0);
        entry_control.append(&entry_row);
        entry_control.append(&location_error);

        let breadcrumbs = gtk::Box::new(gtk::Orientation::Horizontal, 2);
        breadcrumbs.add_css_class("breadcrumbs");
        let breadcrumb_scroller = gtk::ScrolledWindow::builder()
            .child(&breadcrumbs)
            .hscrollbar_policy(gtk::PolicyType::Automatic)
            .vscrollbar_policy(gtk::PolicyType::Never)
            .hexpand(true)
            .build();
        let location_stack = gtk::Stack::builder()
            .hhomogeneous(false)
            .vhomogeneous(false)
            .transition_type(gtk::StackTransitionType::Crossfade)
            .transition_duration(100)
            .build();
        location_stack.add_named(&breadcrumb_scroller, Some("breadcrumbs"));
        location_stack.add_named(&entry_control, Some("entry"));
        location_stack.set_visible_child_name("breadcrumbs");
        location_stack.add_css_class("location-control");
        location_stack.set_hexpand(true);
        location_stack.set_valign(gtk::Align::Center);

        let browser = Browser::new(source);
        let state = Rc::new(ViewState {
            overlay,
            location_stack,
            breadcrumbs,
            location_entry,
            location_error,
            columns_widget,
            scroller,
            columns: RefCell::new(Vec::new()),
            horizontal_scroll_generation: Rc::new(Cell::new(0)),
            peek: RefCell::new(None),
            pending_peek: RefCell::new(None),
            pending_close: RefCell::new(None),
            peek_anchor: RefCell::new(None),
            peek_behavior,
            peek_enabled: Cell::new(true),
            active_rename: RefCell::new(None),
            active_new_folder: RefCell::new(None),
            browser,
        });

        // The observer owns the view state while its window is alive. The window clears
        // the observer on destruction to break this deliberate lifecycle cycle.
        let observer_state = state.clone();
        state
            .browser
            .observe(move |event| observer_state.handle(event));

        let weak_state = Rc::downgrade(&state);
        state.location_entry.connect_activate(move |_| {
            if let Some(state) = weak_state.upgrade() {
                state.submit_location();
            }
        });
        let weak_state = Rc::downgrade(&state);
        confirm_location.connect_clicked(move |_| {
            if let Some(state) = weak_state.upgrade() {
                state.submit_location();
            }
        });
        let weak_state = Rc::downgrade(&state);
        cancel_location.connect_clicked(move |_| {
            if let Some(state) = weak_state.upgrade() {
                state.cancel_location_edit();
            }
        });
        breadcrumb_scroller.set_cursor_from_name(Some("text"));
        let edit_location = gtk::GestureClick::new();
        let weak_state = Rc::downgrade(&state);
        edit_location.connect_released(move |gesture, _, x, y| {
            let clicked_button = gesture
                .widget()
                .and_then(|widget| widget.pick(x, y, gtk::PickFlags::DEFAULT))
                .is_some_and(is_breadcrumb_target);
            if !clicked_button {
                if let Some(state) = weak_state.upgrade() {
                    state.begin_location_edit();
                }
            }
        });
        breadcrumb_scroller.add_controller(edit_location);

        Self { state }
    }

    pub fn widget(&self) -> gtk::Widget {
        self.state.overlay.clone().upcast()
    }

    pub fn navigate(&self, path: impl AsRef<Path>) {
        self.state
            .browser
            .navigate(Location::local(path.as_ref().to_path_buf()));
    }

    pub fn browser(&self) -> Rc<Browser> {
        self.state.browser.clone()
    }

    pub fn set_operation_provider(&self, provider: Rc<dyn OperationProvider>) {
        self.state.browser.set_operation_provider(provider);
    }

    pub fn begin_rename(&self) -> bool {
        self.state.begin_rename()
    }

    pub fn cancel_rename(&self) -> bool {
        self.state.cancel_rename()
    }

    pub fn cancel_new_folder(&self) -> bool {
        self.state.cancel_new_folder()
    }

    pub fn rename_is_active(&self) -> bool {
        self.state.active_rename.borrow().is_some()
    }

    pub fn new_folder_is_active(&self) -> bool {
        self.state.active_new_folder.borrow().is_some()
    }

    pub fn occupied_width(&self) -> i32 {
        self.state
            .columns
            .borrow()
            .iter()
            .map(|column| column.shell.width().max(COLUMN_WIDTH))
            .fold(0, i32::saturating_add)
    }

    pub fn location_widget(&self) -> gtk::Widget {
        self.state.location_stack.clone().upcast()
    }

    pub fn begin_location_edit(&self) {
        self.state.begin_location_edit();
    }

    pub fn location_has_focus(&self) -> bool {
        let entry = self.state.location_entry.upcast_ref::<gtk::Widget>();
        self.state.location_entry.has_focus()
            || self
                .state
                .overlay
                .root()
                .and_then(|root| root.focus())
                .as_ref()
                .is_some_and(|focused| focused == entry || focused.is_ancestor(entry))
    }

    pub fn cancel_location_edit(&self) {
        self.state.cancel_location_edit();
    }

    pub fn set_peek_enabled(&self, enabled: bool) {
        self.state.peek_enabled.set(enabled);
        if !enabled {
            cancel_source(&self.state.pending_peek);
            self.state.browser.close_peek();
        }
    }

    pub fn create_new_folder(&self) {
        let depth = self
            .state
            .focused_column_depth()
            .or_else(|| self.state.browser.active_depth());
        if let Some((depth, location)) = depth.and_then(|depth| {
            self.state
                .browser
                .location_at(depth)
                .map(|location| (depth, location))
        }) {
            self.state.begin_new_folder(depth, location);
        }
    }

    pub fn paste(&self) {
        if let Some(location) = self.state.browser.active_location() {
            self.state.paste_into(location);
        }
    }

    pub fn select_all(&self) {
        if let Some(depth) = self.state.columns.borrow().len().checked_sub(1) {
            self.state.select_all(depth);
        }
    }

    pub fn confirm_delete(&self, permanent: bool) -> bool {
        let entries = self.state.browser.selected_entries();
        if entries.is_empty() {
            return false;
        }
        let in_trash = self
            .state
            .focused_column_depth()
            .and_then(|depth| self.state.browser.location_at(depth))
            .or_else(|| self.state.browser.active_location())
            .as_ref()
            .is_some_and(is_trash_location);
        self.state
            .show_delete_confirmation(entries, permanent || in_trash);
        true
    }

    pub fn empty_trash_requester(&self) -> Rc<dyn Fn()> {
        let weak = Rc::downgrade(&self.state);
        Rc::new(move || {
            if let Some(state) = weak.upgrade() {
                state.show_empty_trash_confirmation();
            }
        })
    }

    pub fn filter_has_focus(&self) -> bool {
        self.state
            .columns
            .borrow()
            .iter()
            .any(|column| column.filter_entry.has_focus())
    }

    pub fn dismiss_focused_filter(&self) -> bool {
        let focused = self.state.overlay.root().and_then(|root| root.focus());
        let columns = self.state.columns.borrow();
        let Some(column) = columns.iter().find(|column| {
            column.filter_entry.has_focus()
                || focused.as_ref().is_some_and(|focused| {
                    focused == column.filter_entry.upcast_ref::<gtk::Widget>()
                        || focused.is_ancestor(&column.filter_entry)
                })
        }) else {
            return false;
        };
        column.filter_button.set_active(false);
        column.list.grab_focus();
        true
    }
}

impl ViewState {
    fn focused_column_depth(&self) -> Option<usize> {
        let focused = self.overlay.root()?.focus()?;
        self.columns.borrow().iter().position(|column| {
            focused == column.shell.clone().upcast::<gtk::Widget>()
                || focused.is_ancestor(&column.shell)
        })
    }

    fn select_all(&self, depth: usize) {
        if let Some(column) = self.columns.borrow().get(depth) {
            column.selection.select_all();
            column.list.grab_focus();
        }
    }

    fn begin_new_folder(self: &Rc<Self>, depth: usize, location: Location) {
        self.cancel_new_folder();
        self.cancel_rename();
        let columns = self.columns.borrow();
        let Some(column) = columns.get(depth) else {
            return;
        };
        column.new_folder_entry.remove_css_class("error");
        column.new_folder_entry.set_tooltip_text(None);
        column.new_folder_entry.set_text("");
        column.new_folder_row.set_visible(true);
        self.active_new_folder.replace(Some(ActiveNewFolder {
            location,
            row: column.new_folder_row.clone(),
            field: column.new_folder_entry.clone(),
        }));
        column.new_folder_entry.grab_focus();
    }

    fn submit_new_folder(self: &Rc<Self>, field: &gtk::Entry) {
        let Some(active) = self
            .active_new_folder
            .take()
            .filter(|active| active.field == *field)
        else {
            return;
        };
        active.row.set_visible(false);
        let name = field.text().to_string();
        field.set_text("");
        if !name.is_empty() {
            self.browser.create_directory(active.location, name);
        }
    }

    fn cancel_new_folder(&self) -> bool {
        let Some(active) = self.active_new_folder.take() else {
            return false;
        };
        active.field.set_text("");
        active.field.remove_css_class("error");
        active.field.set_tooltip_text(None);
        active.row.set_visible(false);
        true
    }

    fn paste_into(self: &Rc<Self>, destination: Location) {
        let Some(display) = gtk::gdk::Display::default() else {
            return;
        };
        let clipboard = display.clipboard();
        let weak = Rc::downgrade(self);
        glib::MainContext::default().spawn_local(async move {
            let result = clipboard
                .read_value_future(gtk::gdk::FileList::static_type(), glib::Priority::DEFAULT)
                .await;
            let files = match result {
                Ok(value) => match value.get::<gtk::gdk::FileList>() {
                    Ok(files) => files.files(),
                    Err(error) => {
                        if let Some(state) = weak.upgrade() {
                            show_error_dialog(
                                &state.overlay,
                                "Unable to paste",
                                &format!("The clipboard does not contain files: {error}"),
                            );
                        }
                        return;
                    }
                },
                Err(error) => {
                    if let Some(state) = weak.upgrade() {
                        show_error_dialog(
                            &state.overlay,
                            "Unable to paste",
                            &format!("The clipboard does not contain files: {error}"),
                        );
                    }
                    return;
                }
            };
            let sources = files
                .into_iter()
                .map(|file| {
                    file.path()
                        .map(Location::local)
                        .unwrap_or_else(|| Location::uri(file.uri()))
                })
                .collect();
            if let Some(state) = weak.upgrade() {
                state.browser.paste(destination, sources);
            }
        });
    }

    fn show_delete_confirmation(self: &Rc<Self>, entries: Vec<FileEntry>, permanent: bool) {
        let Some(window_overlay) = self
            .overlay
            .root()
            .and_downcast::<gtk::Window>()
            .and_then(|window| window.child())
            .and_downcast::<gtk::Overlay>()
        else {
            return;
        };
        let blurred_root = window_overlay.child().and_downcast::<BlurBin>();
        if let Some(root) = blurred_root.as_ref() {
            root.set_blurred(true);
        }

        let count = entries.len();
        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.add_css_class("delete-confirmation");
        content.add_css_class("delete-confirmation-content");
        content.set_halign(gtk::Align::Center);
        content.set_valign(gtk::Align::Center);

        let header = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        header.add_css_class("delete-confirmation-header");
        let symbol = gtk::CenterBox::new();
        symbol.add_css_class("delete-confirmation-symbol");
        symbol.set_size_request(40, 40);
        symbol.set_hexpand(false);
        let symbol_icon = crate::assets::danger_icon(crate::assets::icons::TRASH, 21);
        symbol.set_center_widget(Some(&symbol_icon));
        let heading = gtk::Box::new(gtk::Orientation::Vertical, 1);
        heading.set_hexpand(true);
        let question = gtk::Label::new(Some(&if permanent {
            format!("Permanently delete {}?", item_count_label(count))
        } else {
            format!("Move {} to trash?", item_count_label(count))
        }));
        question.add_css_class("delete-confirmation-title");
        question.set_xalign(0.0);
        let subtitle = gtk::Label::new(Some(&entry_kind_summary(&entries)));
        subtitle.add_css_class("delete-confirmation-subtitle");
        subtitle.set_xalign(0.0);
        heading.append(&question);
        heading.append(&subtitle);
        let close = gtk::Button::new();
        close.add_css_class("delete-confirmation-close");
        close.set_tooltip_text(Some("Cancel"));
        close.set_child(Some(&crate::assets::text_icon(crate::assets::icons::X, 16)));
        header.append(&symbol);
        header.append(&heading);
        header.append(&close);
        content.append(&header);

        let body = gtk::Box::new(gtk::Orientation::Vertical, 12);
        body.add_css_class("delete-confirmation-body");
        let files = gtk::Box::new(gtk::Orientation::Vertical, 3);
        files.add_css_class("delete-confirmation-files");
        for entry in &entries {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            row.add_css_class("delete-confirmation-file");
            let icon = crate::assets::primary_icon(entry_icon(entry), 16);
            let name = gtk::Label::new(Some(&entry.display_name));
            name.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
            name.set_hexpand(true);
            name.set_xalign(0.0);
            name.set_tooltip_text(Some(&entry.location.display_path()));
            let metadata = gtk::Label::new(Some(&if entry.is_directory() {
                "Folder".to_owned()
            } else {
                match entry.size {
                    crate::model::MetadataValue::Known(size) => format_file_size(size),
                    crate::model::MetadataValue::Unknown
                    | crate::model::MetadataValue::Unavailable => "—".to_owned(),
                }
            }));
            metadata.add_css_class("delete-confirmation-file-metadata");
            row.append(&icon);
            row.append(&name);
            row.append(&metadata);
            files.append(&row);
        }
        let file_scroller = gtk::ScrolledWindow::builder()
            .child(&files)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(if count > 10 {
                gtk::PolicyType::Automatic
            } else {
                gtk::PolicyType::Never
            })
            .max_content_height(256)
            .propagate_natural_height(true)
            .build();
        file_scroller.add_css_class("delete-confirmation-list");
        body.append(&file_scroller);
        let explanation = gtk::Label::new(Some(if permanent {
            "These items will be permanently deleted. This action cannot be undone."
        } else {
            "The items will be moved to trash. You can restore them later."
        }));
        explanation.add_css_class("delete-confirmation-explanation");
        explanation.set_wrap(true);
        explanation.set_xalign(0.0);
        body.append(&explanation);
        content.append(&body);

        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        actions.add_css_class("delete-confirmation-actions");
        let action_spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        action_spacer.set_hexpand(true);
        let cancel = gtk::Button::with_label("Cancel");
        cancel.add_css_class("delete-confirmation-cancel");
        let confirm_label = if permanent {
            format!("Permanently delete {}", item_count_label(count))
        } else {
            format!("Move {}", item_count_label(count))
        };
        let confirm_content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        confirm_content.append(&crate::assets::danger_icon(crate::assets::icons::TRASH, 15));
        confirm_content.append(&gtk::Label::new(Some(&confirm_label)));
        let confirm = gtk::Button::builder().child(&confirm_content).build();
        confirm.add_css_class("delete-confirmation-delete");
        actions.append(&action_spacer);
        actions.append(&cancel);
        actions.append(&confirm);
        content.append(&actions);

        let layer = modal_layer(&content);
        window_overlay.add_overlay(&layer);
        let cancelled_layer = layer.clone();
        let cancelled_overlay = window_overlay.clone();
        let cancelled_root = blurred_root.clone();
        cancel.connect_clicked(move |_| {
            dismiss_modal_layer(
                &cancelled_layer,
                &cancelled_overlay,
                cancelled_root.as_ref(),
            );
        });
        let closed_layer = layer.clone();
        let closed_overlay = window_overlay.clone();
        let closed_root = blurred_root.clone();
        close.connect_clicked(move |_| {
            dismiss_modal_layer(&closed_layer, &closed_overlay, closed_root.as_ref());
        });
        let confirmed_layer = layer.clone();
        let confirmed_overlay = window_overlay.clone();
        let confirmed_root = blurred_root.clone();
        let browser = self.browser.clone();
        confirm.connect_clicked(move |_| {
            browser.delete(entries.clone(), permanent);
            dismiss_modal_layer(
                &confirmed_layer,
                &confirmed_overlay,
                confirmed_root.as_ref(),
            );
        });
        let escape = gtk::EventControllerKey::new();
        let escaped_layer = layer.clone();
        let escaped_overlay = window_overlay;
        let escaped_root = blurred_root;
        escape.connect_key_pressed(move |_, key, _, _| {
            if key == gtk::gdk::Key::Escape {
                dismiss_modal_layer(&escaped_layer, &escaped_overlay, escaped_root.as_ref());
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        layer.add_controller(escape);
        cancel.grab_focus();
    }

    fn show_empty_trash_confirmation(self: &Rc<Self>) {
        let Some(window_overlay) = self
            .overlay
            .root()
            .and_downcast::<gtk::Window>()
            .and_then(|window| window.child())
            .and_downcast::<gtk::Overlay>()
        else {
            return;
        };
        let blurred_root = window_overlay.child().and_downcast::<BlurBin>();
        if let Some(root) = blurred_root.as_ref() {
            root.set_blurred(true);
        }

        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.add_css_class("delete-confirmation");
        content.add_css_class("delete-confirmation-content");
        content.set_halign(gtk::Align::Center);
        content.set_valign(gtk::Align::Center);

        let header = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        header.add_css_class("delete-confirmation-header");
        let symbol = gtk::CenterBox::new();
        symbol.add_css_class("delete-confirmation-symbol");
        symbol.set_size_request(40, 40);
        symbol.set_hexpand(false);
        let symbol_icon = crate::assets::danger_icon(crate::assets::icons::TRASH, 21);
        symbol.set_center_widget(Some(&symbol_icon));
        let heading = gtk::Box::new(gtk::Orientation::Vertical, 1);
        heading.set_hexpand(true);
        let question = gtk::Label::new(Some("Empty trash?"));
        question.add_css_class("delete-confirmation-title");
        question.set_xalign(0.0);
        let subtitle = gtk::Label::new(Some("Everything in the trash"));
        subtitle.add_css_class("delete-confirmation-subtitle");
        subtitle.set_xalign(0.0);
        heading.append(&question);
        heading.append(&subtitle);
        let close = gtk::Button::new();
        close.add_css_class("delete-confirmation-close");
        close.set_tooltip_text(Some("Cancel"));
        close.set_child(Some(&crate::assets::text_icon(crate::assets::icons::X, 16)));
        header.append(&symbol);
        header.append(&heading);
        header.append(&close);
        content.append(&header);

        let body = gtk::Box::new(gtk::Orientation::Vertical, 12);
        body.add_css_class("delete-confirmation-body");
        let explanation = gtk::Label::new(Some(
            "All items in the trash will be permanently deleted. This action cannot be undone.",
        ));
        explanation.add_css_class("delete-confirmation-explanation");
        explanation.set_wrap(true);
        explanation.set_xalign(0.0);
        body.append(&explanation);
        content.append(&body);

        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        actions.add_css_class("delete-confirmation-actions");
        let action_spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        action_spacer.set_hexpand(true);
        let cancel = gtk::Button::with_label("Cancel");
        cancel.add_css_class("delete-confirmation-cancel");
        let confirm_content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        confirm_content.append(&crate::assets::danger_icon(crate::assets::icons::TRASH, 15));
        confirm_content.append(&gtk::Label::new(Some("Empty Trash")));
        let confirm = gtk::Button::builder().child(&confirm_content).build();
        confirm.add_css_class("delete-confirmation-delete");
        actions.append(&action_spacer);
        actions.append(&cancel);
        actions.append(&confirm);
        content.append(&actions);

        let layer = modal_layer(&content);
        window_overlay.add_overlay(&layer);
        let cancelled_layer = layer.clone();
        let cancelled_overlay = window_overlay.clone();
        let cancelled_root = blurred_root.clone();
        cancel.connect_clicked(move |_| {
            dismiss_modal_layer(
                &cancelled_layer,
                &cancelled_overlay,
                cancelled_root.as_ref(),
            );
        });
        let closed_layer = layer.clone();
        let closed_overlay = window_overlay.clone();
        let closed_root = blurred_root.clone();
        close.connect_clicked(move |_| {
            dismiss_modal_layer(&closed_layer, &closed_overlay, closed_root.as_ref());
        });
        let confirmed_layer = layer.clone();
        let confirmed_overlay = window_overlay.clone();
        let confirmed_root = blurred_root.clone();
        let browser = self.browser.clone();
        confirm.connect_clicked(move |_| {
            browser.empty_trash();
            dismiss_modal_layer(
                &confirmed_layer,
                &confirmed_overlay,
                confirmed_root.as_ref(),
            );
        });
        let escape = gtk::EventControllerKey::new();
        let escaped_layer = layer.clone();
        let escaped_overlay = window_overlay;
        let escaped_root = blurred_root;
        escape.connect_key_pressed(move |_, key, _, _| {
            if key == gtk::gdk::Key::Escape {
                dismiss_modal_layer(&escaped_layer, &escaped_overlay, escaped_root.as_ref());
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        layer.add_controller(escape);
        cancel.grab_focus();
    }

    fn show_folder_properties(self: &Rc<Self>, location: &Location) {
        self.show_properties(location.clone(), None);
    }

    fn show_entry_properties(self: &Rc<Self>, entry: FileEntry) {
        self.show_properties(entry.location.clone(), Some(entry));
    }

    fn show_properties(self: &Rc<Self>, location: Location, entry: Option<FileEntry>) {
        let Some(window_overlay) = self
            .overlay
            .root()
            .and_downcast::<gtk::Window>()
            .and_then(|window| window.child())
            .and_downcast::<gtk::Overlay>()
        else {
            return;
        };
        let blurred_root = window_overlay.child().and_downcast::<BlurBin>();
        if let Some(root) = blurred_root.as_ref() {
            root.set_blurred(true);
        }
        let is_directory = entry.as_ref().is_none_or(FileEntry::is_directory);
        let name = entry
            .as_ref()
            .map(|entry| entry.display_name.clone())
            .unwrap_or_else(|| location.display_name());
        let icon_name = entry
            .as_ref()
            .map(entry_icon)
            .unwrap_or(crate::assets::icons::FOLDER);

        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.add_css_class("properties-dialog");
        content.add_css_class("properties-content");
        content.set_halign(gtk::Align::Center);
        content.set_valign(gtk::Align::Center);
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        header.add_css_class("properties-header");
        let icon = crate::assets::primary_icon(icon_name, 30);
        icon.add_css_class("properties-icon");
        let heading = gtk::Box::new(gtk::Orientation::Vertical, 1);
        heading.set_hexpand(true);
        let title = gtk::Label::new(Some(&name));
        title.add_css_class("properties-title");
        title.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        title.set_xalign(0.0);
        let kind = gtk::Label::new(Some(if is_directory { "Folder" } else { "File" }));
        kind.add_css_class("properties-kind");
        kind.set_xalign(0.0);
        heading.append(&title);
        heading.append(&kind);
        let close = gtk::Button::new();
        close.add_css_class("properties-close");
        close.set_tooltip_text(Some("Close properties"));
        close.set_child(Some(&crate::assets::primary_icon(
            crate::assets::icons::X,
            15,
        )));
        header.append(&icon);
        header.append(&heading);
        header.append(&close);
        content.append(&header);

        let details = gtk::Box::new(gtk::Orientation::Vertical, 0);
        details.add_css_class("properties-details");
        let location_value = properties_row(&details, "LOCATION", &compact_display_path(&location));
        location_value.set_tooltip_text(Some(&location.display_path()));
        let initial_size = entry
            .as_ref()
            .and_then(|entry| match entry.size {
                crate::model::MetadataValue::Known(size) => Some(format_file_size(size)),
                crate::model::MetadataValue::Unknown | crate::model::MetadataValue::Unavailable => {
                    None
                }
            })
            .unwrap_or_else(|| "—".to_owned());
        let size = properties_row(&details, "SIZE", &initial_size);
        let modified = properties_row(
            &details,
            "MODIFIED",
            &entry
                .as_ref()
                .map(metadata_modified)
                .unwrap_or_else(|| "—".to_owned()),
        );
        let opens_with = properties_row(&details, "OPENS WITH", "—");
        let hidden = properties_row(
            &details,
            "HIDDEN",
            if name.starts_with('.') { "Yes" } else { "No" },
        );
        let _pinned = properties_row(&details, "PINNED", "No");
        content.append(&details);

        let permissions = gtk::Box::new(gtk::Orientation::Vertical, 8);
        permissions.add_css_class("properties-permissions");
        let permissions_header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let permissions_title = gtk::Label::new(Some("PERMISSIONS"));
        permissions_title.add_css_class("properties-section-title");
        permissions_title.set_xalign(0.0);
        permissions_title.set_hexpand(true);
        let permissions_mode = gtk::Label::new(Some("—"));
        permissions_mode.add_css_class("properties-mode");
        permissions_header.append(&permissions_title);
        permissions_header.append(&permissions_mode);
        permissions.append(&permissions_header);
        let owner = permission_row(&permissions, "Owner");
        let group = permission_row(&permissions, "Group");
        let others = permission_row(&permissions, "Others");
        content.append(&permissions);

        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        actions.add_css_class("properties-actions");
        let open = properties_action(crate::assets::icons::EXTERNAL_LINK, "Open");
        let rename = properties_action(crate::assets::icons::PENCIL, "Rename");
        rename.set_sensitive(entry.is_some());
        let pin = properties_action(crate::assets::icons::PIN, "Pin");
        pin.set_sensitive(false);
        pin.set_tooltip_text(Some("Pinned locations are planned"));
        let copy_path = properties_action(crate::assets::icons::COPY, "Copy path");
        actions.append(&open);
        actions.append(&rename);
        actions.append(&pin);
        actions.append(&copy_path);
        content.append(&actions);

        let layer = modal_layer(&content);
        window_overlay.add_overlay(&layer);
        let closing_layer = layer.clone();
        let closing_overlay = window_overlay.clone();
        let closing_root = blurred_root.clone();
        close.connect_clicked(move |_| {
            dismiss_modal_layer(&closing_layer, &closing_overlay, closing_root.as_ref());
        });
        let opening_layer = layer.clone();
        let opening_overlay = window_overlay.clone();
        let opening_root = blurred_root.clone();
        let opening_location = location.clone();
        open.connect_clicked(move |_| {
            open_location(&opening_location, &opening_layer);
            dismiss_modal_layer(&opening_layer, &opening_overlay, opening_root.as_ref());
        });
        let renamed_layer = layer.clone();
        let renamed_overlay = window_overlay.clone();
        let renamed_root = blurred_root.clone();
        let weak = Rc::downgrade(self);
        rename.connect_clicked(move |_| {
            dismiss_modal_layer(&renamed_layer, &renamed_overlay, renamed_root.as_ref());
            let weak = weak.clone();
            glib::idle_add_local_once(move || {
                if let Some(state) = weak.upgrade() {
                    state.begin_rename();
                }
            });
        });
        let copied_location = location.clone();
        copy_path.connect_clicked(move |button| {
            if let Some(display) = gtk::gdk::Display::default() {
                display
                    .clipboard()
                    .set_text(&copied_location.display_path());
                button.set_label("Copied");
            }
        });
        let escape = gtk::EventControllerKey::new();
        let escaped_layer = layer.clone();
        let escaped_overlay = window_overlay.clone();
        let escaped_root = blurred_root.clone();
        escape.connect_key_pressed(move |_, key, _, _| {
            if key == gtk::gdk::Key::Escape {
                dismiss_modal_layer(&escaped_layer, &escaped_overlay, escaped_root.as_ref());
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        layer.add_controller(escape);
        layer.grab_focus();

        let file = gio_file_for_location(&location);
        glib::MainContext::default().spawn_local(async move {
            let Ok(info) = file
                .query_info_future(
                    "standard::content-type,standard::is-hidden,standard::size,time::modified,unix::mode,owner::user,owner::group",
                    gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS,
                    glib::Priority::DEFAULT,
                )
                .await
            else {
                return;
            };
            if !is_directory {
                size.set_text(&format_file_size(info.size().max(0) as u64));
            }
            if let Some(time) = info.modification_date_time() {
                modified.set_text(
                    &time
                        .format("%Y-%m-%d %H:%M")
                        .map(|value| value.to_string())
                        .unwrap_or_else(|_| "—".to_owned()),
                );
            }
            hidden.set_text(if info.is_hidden() { "Yes" } else { "No" });
            if let Some(content_type) = info.content_type() {
                kind.set_text(&gio::content_type_get_description(&content_type));
                if let Some(app) = gio::AppInfo::default_for_type(&content_type, false) {
                    opens_with.set_text(&app.display_name());
                }
            }
            let mode = info.attribute_uint32("unix::mode");
            if mode != 0 {
                permissions_mode.set_text(&format_permissions(mode));
                set_permission_row(&owner, mode, 6);
                set_permission_row(&group, mode, 3);
                set_permission_row(&others, mode, 0);
            }
            owner.0.set_text(info.attribute_string("owner::user").as_deref().unwrap_or("—"));
            group.0.set_text(info.attribute_string("owner::group").as_deref().unwrap_or("—"));
        });
    }

    fn begin_rename(&self) -> bool {
        self.cancel_new_folder();
        let Some((depth, source_position, entry)) = self.browser.rename_item() else {
            return false;
        };
        self.cancel_rename();
        let columns = self.columns.borrow();
        let Some(column) = columns.get(depth) else {
            return false;
        };
        let Some(filtered_position) = filtered_position_for_source(column, source_position) else {
            return false;
        };
        let row = column.bound_rows.borrow().iter().find_map(|bound| {
            let item = bound.item.upgrade()?;
            (item.position() == filtered_position).then(|| bound.row.upgrade())?
        });
        let Some(row) = row else {
            return false;
        };
        let Some(icon) = row.first_child() else {
            return false;
        };
        let Some(label) = icon.next_sibling().and_downcast::<gtk::Label>() else {
            return false;
        };
        let Some(field) = label.next_sibling().and_downcast::<gtk::Entry>() else {
            return false;
        };
        let Some(spacer) = field.next_sibling().and_downcast::<gtk::Box>() else {
            return false;
        };
        field.remove_css_class("error");
        field.set_tooltip_text(None);
        field.set_sensitive(true);
        field.set_text(&entry.display_name);
        label.set_visible(false);
        spacer.set_visible(false);
        field.set_visible(true);
        field.grab_focus();
        field.select_region(0, rename_stem_end(&entry.display_name));
        self.active_rename.replace(Some(ActiveRename {
            entry,
            field,
            label,
            spacer,
        }));
        true
    }

    fn cancel_rename(&self) -> bool {
        let Some(rename) = self.active_rename.take() else {
            return false;
        };
        rename.field.remove_css_class("error");
        rename.field.set_tooltip_text(None);
        rename.field.set_visible(false);
        rename.field.set_sensitive(true);
        rename.label.set_visible(true);
        rename.spacer.set_visible(true);
        true
    }

    fn submit_rename(self: &Rc<Self>, field: &gtk::Entry) {
        let mut active = self.active_rename.borrow_mut();
        let Some(rename) = active.as_mut().filter(|rename| rename.field == *field) else {
            return;
        };
        let new_name = field.text().to_string();
        if new_name == rename.entry.display_name {
            drop(active);
            self.cancel_rename();
            self.browser.focus_active();
            return;
        }
        field.remove_css_class("error");
        field.set_tooltip_text(None);
        field.set_sensitive(false);
        self.browser.rename(rename.entry.clone(), new_name);
    }

    fn begin_location_edit(&self) {
        self.clear_location_error();
        self.location_stack.set_visible_child_name("entry");
        self.location_entry.grab_focus();
        self.location_entry.select_region(0, -1);
    }

    fn cancel_location_edit(&self) {
        self.restore_location_text();
        self.clear_location_error();
        self.location_stack.set_visible_child_name("breadcrumbs");
        self.browser.focus_active();
    }

    fn submit_location(self: &Rc<Self>) {
        let input = self.location_entry.text();
        match self.browser.navigate_input(input.as_str()) {
            Ok(()) => self.clear_location_error(),
            Err(error) => {
                self.location_entry.add_css_class("error");
                self.location_error.set_text(&error.to_string());
                self.location_error.set_visible(true);
                self.location_entry.grab_focus();
            }
        }
    }

    fn restore_location_text(&self) {
        if let Some(location) = self.browser.active_location() {
            self.location_entry.set_text(&location.display_path());
        }
    }

    fn sync_active_location(self: &Rc<Self>) {
        if let Some(location) = self.browser.active_location() {
            self.set_location(&location);
        }
    }

    fn set_location(self: &Rc<Self>, location: &Location) {
        self.location_entry.set_text(&location.display_path());
        while let Some(child) = self.breadcrumbs.first_child() {
            self.breadcrumbs.remove(&child);
        }

        let home = Location::local(glib::home_dir());
        let mut locations = location.breadcrumbs();
        if let Some(home_index) = locations.iter().position(|crumb| crumb == &home) {
            locations.drain(..home_index);
        }
        let starts_at_root = locations
            .first()
            .and_then(Location::native_path)
            .is_some_and(|path| path == Path::new("/"));
        let last = locations.len().saturating_sub(1);
        for (index, crumb) in locations.into_iter().enumerate() {
            if index > 0 && !(starts_at_root && index == 1) {
                let separator = gtk::Label::new(Some("/"));
                separator.add_css_class("breadcrumb-separator");
                self.breadcrumbs.append(&separator);
            }

            let label = if crumb == home {
                "~".to_owned()
            } else {
                crumb.display_name()
            };
            if index == last {
                let current = gtk::Box::new(gtk::Orientation::Horizontal, 2);
                current.add_css_class("current-breadcrumb");
                let current_label = gtk::Label::new(Some(&label));
                current_label.add_css_class("breadcrumb");
                current_label.add_css_class("current");
                current_label.set_tooltip_text(Some(&crumb.display_path()));
                let copy = gtk::Button::builder().tooltip_text("Copy path").build();
                let copy_icon = crate::assets::primary_icon(crate::assets::icons::COPY, 16);
                copy.set_child(Some(&copy_icon));
                copy.add_css_class("copy-path");
                copy.set_has_frame(false);
                copy.set_cursor_from_name(Some("pointer"));
                let copied_path = location.display_path();
                let feedback_generation = Rc::new(Cell::new(0_u64));
                copy.connect_clicked(move |button| {
                    if let Some(display) = gtk::gdk::Display::default() {
                        display.clipboard().set_text(&copied_path);
                    }
                    let generation = feedback_generation.get().saturating_add(1);
                    feedback_generation.set(generation);
                    crate::assets::set_primary_icon(&copy_icon, crate::assets::icons::CHECK);
                    button.set_tooltip_text(Some("Path copied"));
                    let button = button.clone();
                    let copy_icon = copy_icon.clone();
                    let feedback_generation = feedback_generation.clone();
                    glib::timeout_add_local_once(Duration::from_secs(2), move || {
                        if feedback_generation.get() == generation {
                            crate::assets::set_primary_icon(&copy_icon, crate::assets::icons::COPY);
                            button.set_tooltip_text(Some("Copy path"));
                        }
                    });
                });
                current.append(&current_label);
                current.append(&copy);
                self.breadcrumbs.append(&current);
            } else {
                let button = gtk::Button::with_label(&label);
                button.add_css_class("breadcrumb");
                if crumb
                    .native_path()
                    .is_some_and(|path| path == Path::new("/"))
                {
                    button.add_css_class("breadcrumb-root");
                }
                button.set_has_frame(false);
                button.set_tooltip_text(Some(&crumb.display_path()));
                button.set_cursor_from_name(Some("pointer"));
                let weak = Rc::downgrade(self);
                button.connect_clicked(move |_| {
                    if let Some(state) = weak.upgrade() {
                        state.browser.navigate(crumb.clone());
                    }
                });
                self.breadcrumbs.append(&button);
            }
        }
        self.location_stack.set_visible_child_name("breadcrumbs");
    }

    fn clear_location_error(&self) {
        self.location_entry.remove_css_class("error");
        self.location_error.set_visible(false);
        self.location_error.set_text("");
    }

    fn handle(self: &Rc<Self>, event: BrowserEvent) {
        match event {
            BrowserEvent::Reset => {
                self.truncate(0);
                self.clear_location_error();
            }
            BrowserEvent::ColumnsTruncated { len } => {
                self.truncate(len);
                self.sync_active_location();
            }
            BrowserEvent::ColumnAdded { depth, location } => {
                self.set_location(&location);
                self.clear_location_error();
                self.append_column(depth, &location);
            }
            BrowserEvent::EntriesInserted { depth, insertions } => {
                let render_started = Instant::now();
                let entry_count = insertions
                    .iter()
                    .map(|insertion| insertion.entries.len())
                    .sum();
                if let Some(column) = self.columns.borrow().get(depth).cloned() {
                    if entry_count > 0 {
                        column.presentation.show_content();
                    }
                    for insertion in insertions {
                        let labels: Vec<_> =
                            insertion.entries.iter().map(entry_model_value).collect();
                        let labels: Vec<_> = labels.iter().map(String::as_str).collect();
                        column.model.splice(insertion.position as u32, 0, &labels);
                    }
                    let count = column.entry_count.get() + entry_count;
                    column.entry_count.set(count);
                    set_filter_placeholder(&column, count);
                    crate::metrics::mark_batch_rendered(entry_count, render_started);
                }
            }
            BrowserEvent::EntriesReplaced { depth, entries } => {
                if let Some(column) = self.columns.borrow().get(depth).cloned() {
                    if !entries.is_empty() {
                        column.presentation.show_content();
                    }
                    let labels: Vec<_> = entries.iter().map(entry_model_value).collect();
                    let labels: Vec<_> = labels.iter().map(String::as_str).collect();
                    column.model.splice(0, column.model.n_items(), &labels);
                    column.entry_count.set(entries.len());
                    set_filter_placeholder(&column, entries.len());
                }
            }
            BrowserEvent::EntriesSpliced {
                depth,
                splices,
                selected,
            } => {
                if let Some(column) = self.columns.borrow().get(depth) {
                    let mut count = column.entry_count.get();
                    for splice in splices {
                        let labels: Vec<_> = splice.entries.iter().map(entry_model_value).collect();
                        let labels: Vec<_> = labels.iter().map(String::as_str).collect();
                        column
                            .model
                            .splice(splice.position as u32, splice.removed as u32, &labels);
                        count = count
                            .saturating_sub(splice.removed)
                            .saturating_add(splice.entries.len());
                    }
                    column.entry_count.set(count);
                    set_filter_placeholder(column, count);
                    set_column_selection(
                        column,
                        selected
                            .and_then(|position| filtered_position_for_source(column, position))
                            .unwrap_or(gtk::INVALID_LIST_POSITION),
                    );
                    if count == 0 {
                        column.presentation.show_empty();
                    } else {
                        column.presentation.show_content();
                    }
                }
            }
            BrowserEvent::ColumnReloaded { depth } => {
                if let Some(column) = self.columns.borrow().get(depth) {
                    column.model.splice(0, column.model.n_items(), &[]);
                    column.entry_count.set(0);
                    set_filter_placeholder(column, 0);
                    column.spinner.set_visible(true);
                    column.spinner.start();
                    column.presentation.show_loading();
                }
            }
            BrowserEvent::LoadFinished { depth } => {
                if let Some(column) = self.columns.borrow().get(depth) {
                    column.spinner.stop();
                    column.spinner.set_visible(false);
                    if column.entry_count.get() == 0 {
                        column.presentation.show_empty();
                    } else {
                        column.presentation.show_content();
                    }
                }
            }
            BrowserEvent::LoadFailed { depth, message } => {
                if let Some(column) = self.columns.borrow().get(depth) {
                    column.spinner.stop();
                    column.spinner.set_visible(false);
                    column
                        .presentation
                        .show_error(&format!("Unable to read this directory\n{message}"));
                }
            }
            BrowserEvent::PeekStarted { location } => self.append_peek(&location),
            BrowserEvent::PeekEntriesAdded { entries } => {
                if let Some(peek) = self.peek.borrow().as_ref() {
                    if !entries.is_empty() {
                        peek.presentation.show_content();
                    }
                    append_entries(
                        &peek.model,
                        &peek.entry_count,
                        entries,
                        Some(self.peek_behavior.item_limit),
                    );
                }
            }
            BrowserEvent::PeekFinished => {
                if let Some(peek) = self.peek.borrow().as_ref() {
                    peek.spinner.stop();
                    peek.spinner.set_visible(false);
                    if peek.entry_count.get() == 0 {
                        peek.presentation.show_empty();
                    } else {
                        peek.presentation.show_content();
                    }
                }
            }
            BrowserEvent::PeekFailed { message } => {
                if let Some(peek) = self.peek.borrow().as_ref() {
                    peek.spinner.stop();
                    peek.spinner.set_visible(false);
                    peek.presentation
                        .show_error(&format!("Unable to read this directory\n{message}"));
                }
            }
            BrowserEvent::PeekClosed => self.close_peek_visual(),
            BrowserEvent::SelectionSetChanged {
                depth,
                positions,
                focused,
            } => {
                if let Some(column) = self.columns.borrow().get(depth) {
                    let filtered_positions: Vec<_> = positions
                        .into_iter()
                        .filter_map(|position| filtered_position_for_source(column, position))
                        .collect();
                    set_column_selections(column, &filtered_positions);
                    if let Some(focused) = filtered_position_for_source(column, focused) {
                        column
                            .list
                            .scroll_to(focused, gtk::ListScrollFlags::FOCUS, None);
                    }
                    column.list.grab_focus();
                }
            }
            BrowserEvent::FocusChanged { depth, position } => {
                if let Some(column) = self.columns.borrow().get(depth) {
                    if let Some(filtered_position) =
                        position.and_then(|position| filtered_position_for_source(column, position))
                    {
                        set_column_selection(column, filtered_position);
                        column
                            .list
                            .scroll_to(filtered_position, gtk::ListScrollFlags::FOCUS, None);
                    }
                    column.list.grab_focus();
                }
            }
            BrowserEvent::PreviewRequested { .. } => {}
            BrowserEvent::OpenRequested { location } => {
                open_location(&location, &self.overlay);
            }
            BrowserEvent::RenameCompleted => {
                self.cancel_rename();
                self.browser.focus_active();
            }
            BrowserEvent::RenameFailed { message } => {
                if let Some(rename) = self.active_rename.borrow().as_ref() {
                    rename.field.set_sensitive(true);
                    rename.field.add_css_class("error");
                    rename.field.set_tooltip_text(Some(&message));
                    rename.field.grab_focus();
                }
            }
            BrowserEvent::OperationFailed { message } => {
                show_error_dialog(&self.overlay, "Unable to complete operation", &message);
            }
            BrowserEvent::NavigationRejected { message } => {
                show_error_dialog(&self.overlay, "Unable to open directory", &message);
            }
        }
        self.refresh_active_path_rows();
    }

    fn refresh_active_path_rows(&self) {
        for (depth, column) in self.columns.borrow().iter().enumerate() {
            let active = self
                .browser
                .active_child_position(depth)
                .and_then(|position| filtered_position_for_source(column, position));
            column.bound_rows.borrow_mut().retain(|bound| {
                let (Some(item), Some(row)) = (bound.item.upgrade(), bound.row.upgrade()) else {
                    return false;
                };
                set_active_path_style(&row, active == Some(item.position()));
                true
            });
        }
    }

    fn append_column(self: &Rc<Self>, depth: usize, location: &Location) {
        let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
        column.add_css_class("directory-column");
        column.set_hexpand(true);
        column.set_vexpand(true);

        let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        header.add_css_class("column-header");
        let heading = gtk::Label::new(Some(&location.display_name()));
        heading.set_xalign(0.0);
        heading.set_hexpand(true);
        heading.set_tooltip_text(Some(&location.display_path()));
        let spinner = gtk::Spinner::new();
        spinner.start();
        header.append(&heading);
        header.append(&spinner);
        let header_actions = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        header_actions.add_css_class("column-header-actions");
        header_actions.append(&column_sort_direction_toggle(&self.browser, depth));
        header_actions.append(&column_sort_menu(&self.browser, depth));

        let filter_entry = gtk::Entry::builder()
            .placeholder_text("Filter 0 items…")
            .has_frame(false)
            .hexpand(true)
            .build();
        filter_entry.add_css_class("column-filter-entry");
        let filter_icon = crate::assets::primary_icon(crate::assets::icons::FUNNEL, 16);
        let filter_control = gtk::Box::new(gtk::Orientation::Horizontal, 7);
        filter_control.add_css_class("column-filter");
        filter_control.append(&filter_icon);
        filter_control.append(&filter_entry);
        let filter_revealer = gtk::Revealer::builder()
            .transition_type(gtk::RevealerTransitionType::SlideDown)
            .child(&filter_control)
            .build();
        let filter_button = gtk::ToggleButton::builder()
            .tooltip_text("Filter this pane")
            .build();
        filter_button.set_child(Some(&crate::assets::text_icon(
            crate::assets::icons::FUNNEL,
            16,
        )));
        filter_button.add_css_class("column-header-action");
        let shown_filter = filter_revealer.clone();
        let focused_filter = filter_entry.clone();
        filter_button.connect_toggled(move |button| {
            shown_filter.set_reveal_child(button.is_active());
            if button.is_active() {
                focused_filter.grab_focus();
            } else {
                focused_filter.set_text("");
            }
        });
        header_actions.append(&filter_button);
        if depth > 0 {
            let close = gtk::Button::builder()
                .tooltip_text("Close this pane")
                .build();
            close.set_child(Some(&crate::assets::text_icon(crate::assets::icons::X, 16)));
            close.add_css_class("column-header-action");
            let weak_browser = Rc::downgrade(&self.browser);
            close.connect_clicked(move |_| {
                if let Some(browser) = weak_browser.upgrade() {
                    browser.close_column(depth);
                }
            });
            header_actions.append(&close);
        }
        header.append(&header_actions);
        column.append(&header);
        column.append(&filter_revealer);

        let entry_count = Rc::new(Cell::new(0));
        let model = gtk::StringList::new(&[]);
        let filter_query = Rc::new(RefCell::new(String::new()));
        let query = filter_query.clone();
        let filter = gtk::CustomFilter::new(move |item| {
            let Some(item) = item.downcast_ref::<gtk::StringObject>() else {
                return false;
            };
            let query = query.borrow();
            query.is_empty()
                || model_display_name(&item.string())
                    .to_lowercase()
                    .contains(query.as_str())
        });
        let filtered_model = gtk::FilterListModel::new(Some(model.clone()), Some(filter.clone()));
        let selection = gtk::MultiSelection::new(Some(filtered_model.clone()));
        let syncing_selection = Rc::new(Cell::new(false));
        let modified_selection = Rc::new(Cell::new(false));
        let focused_filtered = Rc::new(Cell::new(None::<u32>));
        let weak_browser = Rc::downgrade(&self.browser);
        let source_for_selection = model.clone();
        let filtered_for_selection = filtered_model.clone();
        let syncing_selection_changed = syncing_selection.clone();
        let focused_filtered_changed = focused_filtered.clone();
        selection.connect_selection_changed(move |selection, position, count| {
            if syncing_selection_changed.get() {
                return;
            }
            let filtered_positions = bitset_positions(&selection.selection());
            let source_positions: Vec<_> = filtered_positions
                .iter()
                .filter_map(|position| {
                    source_position_for_filtered(
                        &source_for_selection,
                        &filtered_for_selection,
                        *position,
                    )
                })
                .collect();
            let changed_end = position.saturating_add(count);
            let focused = filtered_positions
                .iter()
                .rev()
                .copied()
                .find(|candidate| *candidate >= position && *candidate < changed_end)
                .or_else(|| {
                    focused_filtered_changed
                        .get()
                        .filter(|candidate| filtered_positions.contains(candidate))
                })
                .or_else(|| filtered_positions.last().copied());
            focused_filtered_changed.set(focused);
            let focused_source = focused.and_then(|position| {
                source_position_for_filtered(
                    &source_for_selection,
                    &filtered_for_selection,
                    position,
                )
            });
            if let Some(browser) = weak_browser.upgrade() {
                browser.set_selection(depth, &source_positions, focused_source);
            }
        });
        filter_entry.connect_changed(move |entry| {
            *filter_query.borrow_mut() = entry.text().to_lowercase();
            filter.changed(gtk::FilterChange::Different);
        });

        let factory = gtk::SignalListItemFactory::new();
        let bound_rows: Rc<RefCell<Vec<BoundRow>>> = Rc::new(RefCell::new(Vec::new()));
        let rows_for_setup = bound_rows.clone();
        let weak_state = Rc::downgrade(self);
        let modified_selection_for_rows = modified_selection.clone();
        let selection_for_rows = selection.clone();
        let mouse_selection_anchor = Rc::new(Cell::new(None::<u32>));
        let source_for_hover = model.clone();
        let filtered_for_hover = filtered_model.clone();
        let mouse_selection_anchor_for_background = mouse_selection_anchor.clone();
        factory.connect_setup(move |_, item| {
            let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            row.add_css_class("file-row");
            let icon = gtk::Image::new();
            icon.add_css_class("file-icon");
            icon.set_pixel_size(17);
            let label = gtk::Label::builder()
                .halign(gtk::Align::Fill)
                .xalign(0.0)
                .hexpand(false)
                .max_width_chars(24)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .build();
            let rename = gtk::Entry::new();
            rename.add_css_class("inline-rename");
            rename.set_hexpand(true);
            rename.set_visible(false);
            let weak_state_for_rename = weak_state.clone();
            rename.connect_activate(move |field| {
                if let Some(state) = weak_state_for_rename.upgrade() {
                    state.submit_rename(field);
                }
            });
            let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            spacer.add_css_class("file-row-spacer");
            spacer.set_hexpand(true);
            let size = gtk::Label::new(None);
            size.add_css_class("file-size");
            size.set_xalign(1.0);
            let chevron = crate::assets::primary_icon(crate::assets::icons::CHEVRON_RIGHT, 15);
            chevron.add_css_class("file-chevron");
            row.append(&icon);
            row.append(&label);
            row.append(&rename);
            row.append(&spacer);
            row.append(&size);
            row.append(&chevron);
            let motion = gtk::EventControllerMotion::new();
            let list_item = item.clone();
            let anchor: gtk::Widget = row.clone().upcast();
            let weak_state_for_enter = weak_state.clone();
            let source_for_enter = source_for_hover.clone();
            let filtered_for_enter = filtered_for_hover.clone();
            motion.connect_enter(move |_, _, _| {
                if let Some(state) = weak_state_for_enter.upgrade() {
                    let source_position = source_position_for_filtered(
                        &source_for_enter,
                        &filtered_for_enter,
                        list_item.position(),
                    );
                    let entry = source_position
                        .and_then(|position| state.browser.entry_at(depth, position));
                    if let Some(entry) = entry {
                        if entry.is_directory() {
                            state.schedule_peek(depth, entry.location, anchor.clone());
                        } else {
                            cancel_source(&state.pending_peek);
                            state.browser.close_peek();
                        }
                    }
                }
            });
            let weak_state_for_leave = weak_state.clone();
            motion.connect_leave(move |_| {
                if let Some(state) = weak_state_for_leave.upgrade() {
                    state.schedule_close_peek();
                }
            });
            row.add_controller(motion);

            let drag = gtk::DragSource::builder()
                .actions(gtk::gdk::DragAction::COPY | gtk::gdk::DragAction::MOVE)
                .build();
            let weak_state_for_drag = weak_state.clone();
            let dragged_item = item.clone();
            let source_for_drag = source_for_hover.clone();
            let filtered_for_drag = filtered_for_hover.clone();
            drag.connect_prepare(move |source, x, y| {
                let state = weak_state_for_drag.upgrade()?;
                let source_position = source_position_for_filtered(
                    &source_for_drag,
                    &filtered_for_drag,
                    dragged_item.position(),
                )?;
                let entry = state.browser.entry_at(depth, source_position)?;
                let selected = state.browser.selected_entries();
                let entries = if selected
                    .iter()
                    .any(|selected| selected.location == entry.location)
                {
                    selected
                } else {
                    vec![entry]
                };
                let paintable = gtk::WidgetPaintable::new(source.widget().as_ref());
                source.set_icon(Some(&paintable), x.round() as i32, y.round() as i32);
                file_drag_content(&entries)
            });
            let dragged_row = row.clone();
            drag.connect_drag_begin(move |_, _| dragged_row.add_css_class("dragging"));
            let dragged_row = row.clone();
            drag.connect_drag_end(move |_, _, _| dragged_row.remove_css_class("dragging"));
            row.add_controller(drag);

            let drop = gtk::DropTarget::new(
                gtk::gdk::FileList::static_type(),
                gtk::gdk::DragAction::COPY | gtk::gdk::DragAction::MOVE,
            );
            let weak_state_for_accept = weak_state.clone();
            let accepted_item = item.clone();
            let source_for_accept = source_for_hover.clone();
            let filtered_for_accept = filtered_for_hover.clone();
            drop.connect_accept(move |_, offered| {
                let Some(state) = weak_state_for_accept.upgrade() else {
                    return false;
                };
                let entry = source_position_for_filtered(
                    &source_for_accept,
                    &filtered_for_accept,
                    accepted_item.position(),
                )
                .and_then(|position| state.browser.entry_at(depth, position));
                entry.is_some_and(|entry| {
                    entry.is_directory()
                        && offered
                            .formats()
                            .contains_type(gtk::gdk::FileList::static_type())
                })
            });
            let weak_state_for_drop = weak_state.clone();
            let dropped_item = item.clone();
            let source_for_drop = source_for_hover.clone();
            let filtered_for_drop = filtered_for_hover.clone();
            drop.connect_drop(move |target, value, _, _| {
                let Some(state) = weak_state_for_drop.upgrade() else {
                    return false;
                };
                let Some(destination) = source_position_for_filtered(
                    &source_for_drop,
                    &filtered_for_drop,
                    dropped_item.position(),
                )
                .and_then(|position| state.browser.entry_at(depth, position))
                .filter(FileEntry::is_directory)
                .map(|entry| entry.location) else {
                    return false;
                };
                transfer_dropped_files(&state, target, value, destination)
            });
            row.add_controller(drop);

            let selection_click = gtk::GestureClick::new();
            selection_click.set_button(1);
            selection_click.set_propagation_phase(gtk::PropagationPhase::Capture);
            let clicked_item = item.clone();
            let selection_for_click = selection_for_rows.clone();
            let selection_anchor_for_click = mouse_selection_anchor.clone();
            let modified_for_click = modified_selection_for_rows.clone();
            let weak_state_for_click = weak_state.clone();
            let source_for_click = source_for_hover.clone();
            let filtered_for_click = filtered_for_hover.clone();
            selection_click.connect_pressed(move |gesture, press_count, _, _| {
                let position = clicked_item.position();
                if position == gtk::INVALID_LIST_POSITION {
                    return;
                }
                let modifiers = gesture.current_event_state();
                let control = modifiers.contains(gtk::gdk::ModifierType::CONTROL_MASK);
                let shift = modifiers.contains(gtk::gdk::ModifierType::SHIFT_MASK);
                modified_for_click.set(control || shift);
                if shift {
                    let anchor = selection_anchor_for_click.get().unwrap_or(position);
                    let start = anchor.min(position);
                    let count = anchor.max(position).saturating_sub(start) + 1;
                    selection_for_click.select_range(start, count, true);
                } else if control {
                    selection_anchor_for_click.set(Some(position));
                    if selection_for_click.is_selected(position) {
                        selection_for_click.unselect_item(position);
                    } else {
                        selection_for_click.select_item(position, false);
                    }
                } else {
                    selection_anchor_for_click.set(Some(position));
                    selection_for_click.select_item(position, true);
                }
                modified_for_click.set(false);

                let source_position =
                    source_position_for_filtered(&source_for_click, &filtered_for_click, position);
                if let (Some(state), Some(source_position)) =
                    (weak_state_for_click.upgrade(), source_position)
                {
                    if press_count == 2 {
                        state.browser.activate(depth, source_position);
                    } else if !control && !shift {
                        state.browser.preview(depth, source_position);
                    }
                }
            });
            row.add_controller(selection_click);
            item.set_child(Some(&row));
            let weak_item = glib::WeakRef::new();
            weak_item.set(Some(item));
            let weak_row = glib::WeakRef::new();
            weak_row.set(Some(&row));
            rows_for_setup.borrow_mut().push(BoundRow {
                item: weak_item,
                row: weak_row,
            });
        });
        let source_for_bind = model.clone();
        let filtered_for_bind = filtered_model.clone();
        let weak_browser_for_bind = Rc::downgrade(&self.browser);
        factory.connect_bind(move |_, item| {
            let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            let Some(value) = item.item().and_downcast::<gtk::StringObject>() else {
                return;
            };
            let Some(row) = item.child().and_downcast::<gtk::Box>() else {
                return;
            };
            let Some(icon) = row.first_child().and_downcast::<gtk::Image>() else {
                return;
            };
            let Some(label) = icon.next_sibling().and_downcast::<gtk::Label>() else {
                return;
            };
            let Some(rename) = label.next_sibling().and_downcast::<gtk::Entry>() else {
                return;
            };
            let Some(spacer) = rename.next_sibling().and_downcast::<gtk::Box>() else {
                return;
            };
            let Some(size) = spacer.next_sibling().and_downcast::<gtk::Label>() else {
                return;
            };
            let Some(chevron) = size.next_sibling().and_downcast::<gtk::Image>() else {
                return;
            };
            label.set_label(model_display_name(&value.string()));
            rename.set_visible(false);
            label.set_visible(true);
            spacer.set_visible(true);
            let source_position =
                source_position_for_filtered(&source_for_bind, &filtered_for_bind, item.position());
            let browser = weak_browser_for_bind.upgrade();
            let entry =
                source_position.and_then(|position| browser.as_ref()?.entry_at(depth, position));
            let active = source_position.is_some_and(|position| {
                browser
                    .as_ref()
                    .and_then(|browser| browser.active_child_position(depth))
                    == Some(position)
            });
            set_active_path_style(&row, active);
            if let Some(entry) = entry.as_ref() {
                crate::assets::set_primary_icon(&icon, entry_icon(entry));
                icon.set_opacity(if entry.is_directory() { 1.0 } else { 0.72 });
                chevron.set_visible(entry.is_directory());
            } else {
                crate::assets::set_primary_icon(&icon, crate::assets::icons::DOCUMENTS);
                icon.set_opacity(0.72);
                chevron.set_visible(false);
            }
            let size_text = entry
                .filter(|entry| !entry.is_directory())
                .and_then(|entry| match entry.size {
                    crate::model::MetadataValue::Known(bytes) => Some(format_file_size(bytes)),
                    crate::model::MetadataValue::Unknown
                    | crate::model::MetadataValue::Unavailable => None,
                })
                .unwrap_or_default();
            size.set_label(&size_text);
        });

        let list = gtk::ListView::new(Some(selection.clone()), Some(factory));
        list.add_css_class("file-list");
        list.set_enable_rubberband(false);
        list.set_single_click_activate(false);
        list.set_vexpand(true);

        let marquee_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        marquee_box.add_css_class("file-marquee");
        marquee_box.set_can_target(false);
        marquee_box.set_halign(gtk::Align::Start);
        marquee_box.set_valign(gtk::Align::Start);
        marquee_box.set_visible(false);
        self.overlay.add_overlay(&marquee_box);

        let marquee_active = Rc::new(Cell::new(false));
        let marquee_origin = Rc::new(Cell::new((0.0, 0.0)));
        let marquee_initial = Rc::new(RefCell::new(gtk::Bitset::new_empty()));
        let marquee_modifiers = Rc::new(Cell::new((false, false)));
        let marquee = gtk::GestureDrag::new();
        marquee.set_button(1);
        marquee.set_propagation_phase(gtk::PropagationPhase::Capture);
        let active_for_begin = marquee_active.clone();
        let origin_for_begin = marquee_origin.clone();
        let initial_for_begin = marquee_initial.clone();
        let modifiers_for_begin = marquee_modifiers.clone();
        let selection_for_begin = selection.clone();
        let marquee_box_for_begin = marquee_box.clone();
        marquee.connect_drag_begin(move |gesture, x, y| {
            let starts_on_row = gesture
                .widget()
                .and_then(|widget| widget.pick(x, y, gtk::PickFlags::DEFAULT))
                .is_some_and(is_file_row_target);
            let force_marquee = gesture
                .current_event_state()
                .contains(gtk::gdk::ModifierType::ALT_MASK);
            let can_start = force_marquee || !starts_on_row;
            active_for_begin.set(can_start);
            if !can_start {
                return;
            }
            gesture.set_state(gtk::EventSequenceState::Claimed);
            marquee_box_for_begin.set_visible(true);
            origin_for_begin.set((x, y));
            initial_for_begin.replace(selection_for_begin.selection().copy());
            let modifiers = gesture.current_event_state();
            modifiers_for_begin.set((
                modifiers.contains(gtk::gdk::ModifierType::CONTROL_MASK),
                modifiers.contains(gtk::gdk::ModifierType::SHIFT_MASK),
            ));
        });
        let active_for_update = marquee_active.clone();
        let origin_for_update = marquee_origin.clone();
        let initial_for_update = marquee_initial.clone();
        let modifiers_for_update = marquee_modifiers.clone();
        let selection_for_marquee = selection.clone();
        let rows_for_marquee = bound_rows.clone();
        let list_for_marquee = list.clone();
        let overlay_for_marquee = self.overlay.clone();
        let marquee_box_for_update = marquee_box.clone();
        marquee.connect_drag_update(move |_, offset_x, offset_y| {
            if !active_for_update.get() {
                return;
            }
            let (origin_x, origin_y) = origin_for_update.get();
            let current_x = origin_x + offset_x;
            let current_y = origin_y + offset_y;
            let left = origin_x.min(current_x);
            let right = origin_x.max(current_x);
            let top = origin_y.min(current_y);
            let bottom = origin_y.max(current_y);
            if let Some(list_bounds) = list_for_marquee.compute_bounds(&overlay_for_marquee) {
                marquee_box_for_update
                    .set_margin_start((f64::from(list_bounds.x()) + left).round().max(0.0) as i32);
                marquee_box_for_update
                    .set_margin_top((f64::from(list_bounds.y()) + top).round().max(0.0) as i32);
                marquee_box_for_update.set_size_request(
                    (right - left).round().max(1.0) as i32,
                    (bottom - top).round().max(1.0) as i32,
                );
            }
            let initial = initial_for_update.borrow();
            let (control, shift) = modifiers_for_update.get();
            let selected = if control || shift {
                initial.copy()
            } else {
                gtk::Bitset::new_empty()
            };
            rows_for_marquee.borrow_mut().retain(|bound| {
                let (Some(item), Some(row)) = (bound.item.upgrade(), bound.row.upgrade()) else {
                    return false;
                };
                let Some(bounds) = row.compute_bounds(&list_for_marquee) else {
                    return true;
                };
                let intersects = f64::from(bounds.x()) < right
                    && f64::from(bounds.x() + bounds.width()) > left
                    && f64::from(bounds.y()) < bottom
                    && f64::from(bounds.y() + bounds.height()) > top;
                let position = item.position();
                if intersects && position != gtk::INVALID_LIST_POSITION {
                    if control && initial.contains(position) {
                        selected.remove(position);
                    } else {
                        selected.add(position);
                    }
                }
                true
            });
            let mask = gtk::Bitset::new_range(0, selection_for_marquee.n_items());
            selection_for_marquee.set_selection(&selected, &mask);
        });
        let active_for_end = marquee_active.clone();
        let marquee_box_for_end = marquee_box.clone();
        marquee.connect_drag_end(move |_, _, _| {
            active_for_end.set(false);
            marquee_box_for_end.set_visible(false);
        });
        let clear_selection = gtk::GestureClick::new();
        clear_selection.set_button(1);
        let background_press = Rc::new(Cell::new((0.0, 0.0)));
        let background_press_start = background_press.clone();
        clear_selection.connect_pressed(move |_, _, x, y| background_press_start.set((x, y)));
        let selection_for_background = selection.clone();
        clear_selection.connect_released(move |gesture, _, x, y| {
            let (start_x, start_y) = background_press.get();
            if (x - start_x).abs() > 3.0 || (y - start_y).abs() > 3.0 {
                return;
            }
            let target = gesture
                .widget()
                .and_then(|widget| widget.pick(x, y, gtk::PickFlags::DEFAULT));
            if !target.is_some_and(is_file_row_target) {
                selection_for_background.unselect_all();
                mouse_selection_anchor_for_background.set(None);
            }
        });
        list.add_controller(marquee);
        list.add_controller(clear_selection);
        let selection_keys = gtk::EventControllerKey::new();
        selection_keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let modified_for_key = modified_selection.clone();
        selection_keys.connect_key_pressed(move |_, _, _, modifiers| {
            modified_for_key.set(
                modifiers.contains(gtk::gdk::ModifierType::CONTROL_MASK)
                    || modifiers.contains(gtk::gdk::ModifierType::SHIFT_MASK),
            );
            glib::Propagation::Proceed
        });
        let modified_for_key = modified_selection.clone();
        selection_keys.connect_key_released(move |_, _, _, _| {
            modified_for_key.set(false);
        });
        list.add_controller(selection_keys);

        let weak_browser = Rc::downgrade(&self.browser);
        let source_for_activation = model.clone();
        let filtered_for_activation = filtered_model.clone();
        list.connect_activate(move |_, position| {
            let source_position = source_position_for_filtered(
                &source_for_activation,
                &filtered_for_activation,
                position,
            );
            if let (Some(browser), Some(source_position)) =
                (weak_browser.upgrade(), source_position)
            {
                browser.activate(depth, source_position);
            }
        });

        let scroll = gtk::ScrolledWindow::builder()
            .child(&list)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build();
        let retry = gtk::Button::with_label("Retry");
        retry.add_css_class("retry-button");
        let weak_browser = Rc::downgrade(&self.browser);
        retry.connect_clicked(move |_| {
            if let Some(browser) = weak_browser.upgrade() {
                browser.retry_column(depth);
            }
        });
        let new_folder_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        new_folder_row.add_css_class("file-row");
        new_folder_row.add_css_class("new-folder-row");
        new_folder_row.set_visible(false);
        let new_folder_icon = crate::assets::primary_icon(crate::assets::icons::FOLDER, 17);
        new_folder_icon.add_css_class("file-icon");
        let new_folder_entry = gtk::Entry::new();
        new_folder_entry.add_css_class("inline-rename");
        new_folder_entry.set_hexpand(true);
        new_folder_row.append(&new_folder_icon);
        new_folder_row.append(&new_folder_entry);
        let weak_state = Rc::downgrade(self);
        new_folder_entry.connect_activate(move |field| {
            if let Some(state) = weak_state.upgrade() {
                state.submit_new_folder(field);
            }
        });
        let new_folder_focus = gtk::EventControllerFocus::new();
        let weak_state = Rc::downgrade(self);
        let field = new_folder_entry.clone();
        new_folder_focus.connect_leave(move |_| {
            if let Some(state) = weak_state.upgrade() {
                state.submit_new_folder(&field);
            }
        });
        new_folder_entry.add_controller(new_folder_focus);

        let presentation = LoadPresentation::new(&scroll, Some(retry));
        install_directory_drop_target(self, &presentation.stack, location.clone());
        install_folder_context_menu(self, &presentation.stack, &list, depth, location.clone());
        install_item_context_menu(
            self,
            &list,
            &selection,
            &bound_rows,
            &model,
            &filtered_model,
            depth,
        );
        column.append(&new_folder_row);
        column.append(&presentation.stack);

        let shell = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        shell.set_size_request(COLUMN_WIDTH, -1);
        shell.set_vexpand(true);
        shell.set_overflow(gtk::Overflow::Hidden);
        let column_overlay = gtk::Overlay::new();
        column_overlay.set_child(Some(&column));
        column_overlay.set_hexpand(true);
        column_overlay.set_vexpand(true);
        shell.append(&column_overlay);
        let resize_handle = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        resize_handle.add_css_class("column-resize-handle");
        resize_handle.set_width_request(7);
        resize_handle.set_cursor_from_name(Some("col-resize"));
        let resize = gtk::GestureDrag::new();
        resize.set_button(1);
        let resize_start = Rc::new(Cell::new(COLUMN_WIDTH));
        let pointer_start = Rc::new(Cell::new(None));
        let shell_for_resize_start = shell.clone();
        let resize_start_for_begin = resize_start.clone();
        let pointer_start_for_begin = pointer_start.clone();
        resize.connect_drag_begin(move |gesture, _, _| {
            resize_start_for_begin.set(shell_for_resize_start.width().max(COLUMN_WIDTH));
            if let Some((pointer_x, _)) = gesture.current_event().and_then(|event| event.position())
            {
                pointer_start_for_begin.set(Some(pointer_x));
            }
            gesture.set_state(gtk::EventSequenceState::Claimed);
        });
        let shell_for_resize = shell.clone();
        resize.connect_drag_update(move |gesture, fallback_offset_x, _| {
            let pointer_x = gesture
                .current_event()
                .and_then(|event| event.position())
                .map(|(pointer_x, _)| pointer_x);
            let offset_x = pointer_start
                .get()
                .zip(pointer_x)
                .map_or(fallback_offset_x, |(start, current)| current - start);
            shell_for_resize
                .set_size_request(resized_column_width(resize_start.get(), offset_x), -1);
        });
        resize_handle.add_controller(resize);
        resize_handle.set_halign(gtk::Align::End);
        resize_handle.set_valign(gtk::Align::Fill);
        column_overlay.add_overlay(&resize_handle);
        let animation_generation = Rc::new(Cell::new(0));
        let previous = depth
            .checked_sub(1)
            .and_then(|previous| self.columns.borrow().get(previous).cloned())
            .map(|column| column.shell);
        self.columns_widget
            .insert_child_after(&shell, previous.as_ref());
        self.columns.borrow_mut().push(ColumnView {
            shell: shell.clone(),
            animation_generation: animation_generation.clone(),
            presentation,
            model,
            filtered_model,
            filter_entry,
            filter_button,
            selection,
            syncing_selection,
            list,
            marquee: marquee_box,
            bound_rows,
            entry_count,
            spinner,
            new_folder_row,
            new_folder_entry,
        });

        self.refresh_active_path_rows();
        animate_column_entry(&shell, &column, &animation_generation);
        self.reveal_column(shell);
    }

    fn reveal_column(self: &Rc<Self>, shell: gtk::Box) {
        let animation_id = self.horizontal_scroll_generation.get().saturating_add(1);
        self.horizontal_scroll_generation.set(animation_id);
        let weak = Rc::downgrade(self);
        let measured_shell = shell;
        let _tick = self.scroller.add_tick_callback(move |_, _| {
            let Some(state) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            if state.horizontal_scroll_generation.get() != animation_id
                || measured_shell.parent().is_none()
            {
                return glib::ControlFlow::Break;
            }
            let adjustment = state.scroller.hadjustment();
            if measured_shell.width() <= 0 || adjustment.page_size() <= 0.0 {
                return glib::ControlFlow::Continue;
            }
            let Some(bounds) = measured_shell.compute_bounds(&state.columns_widget) else {
                return glib::ControlFlow::Continue;
            };
            let target = horizontal_reveal_target(
                adjustment.value(),
                adjustment.page_size(),
                adjustment.lower(),
                adjustment.upper(),
                f64::from(bounds.x()),
                f64::from(bounds.x() + bounds.width()),
            );
            animate_horizontal_scroll(
                &state.scroller,
                &adjustment,
                target,
                &state.horizontal_scroll_generation,
                animation_id,
            );
            glib::ControlFlow::Break
        });
    }

    fn schedule_peek(
        self: &Rc<Self>,
        origin_depth: usize,
        location: Location,
        anchor: gtk::Widget,
    ) {
        if !self.peek_enabled.get() {
            return;
        }
        cancel_source(&self.pending_peek);
        cancel_source(&self.pending_close);
        if self
            .peek
            .borrow()
            .as_ref()
            .is_some_and(|peek| peek.location == location)
        {
            return;
        }
        self.peek_anchor.replace(Some(anchor));

        let weak_state = Rc::downgrade(self);
        let source = glib::timeout_add_local_once(self.peek_behavior.open_delay, move || {
            if let Some(state) = weak_state.upgrade() {
                state.pending_peek.take();
                state.browser.begin_peek(origin_depth, location);
            }
        });
        self.pending_peek.replace(Some(source));
    }

    fn schedule_close_peek(self: &Rc<Self>) {
        cancel_source(&self.pending_peek);
        cancel_source(&self.pending_close);

        let weak_state = Rc::downgrade(self);
        let source = glib::timeout_add_local_once(self.peek_behavior.close_delay, move || {
            if let Some(state) = weak_state.upgrade() {
                state.pending_close.take();
                state.browser.close_peek();
            }
        });
        self.pending_close.replace(Some(source));
    }

    fn append_peek(self: &Rc<Self>, location: &Location) {
        let anchor = self.peek_anchor.take();
        self.close_peek_visual();
        let Some(anchor) = anchor else {
            self.browser.close_peek();
            return;
        };

        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.set_size_request(256, -1);
        content.set_overflow(gtk::Overflow::Hidden);

        let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        header.add_css_class("column-header");
        let heading = gtk::Label::new(Some(&location.display_name()));
        heading.set_xalign(0.0);
        heading.set_hexpand(true);
        let spinner = gtk::Spinner::new();
        spinner.start();
        header.append(&heading);
        header.append(&spinner);
        content.append(&header);

        let entry_count = Rc::new(Cell::new(0));
        let model = gtk::StringList::new(&[]);
        let selection = gtk::NoSelection::new(Some(model.clone()));
        let factory = basic_label_factory();
        let list = gtk::ListView::new(Some(selection), Some(factory));
        list.add_css_class("file-list");
        let weak_browser = Rc::downgrade(&self.browser);
        list.connect_activate(move |_, _| {
            if let Some(browser) = weak_browser.upgrade() {
                browser.commit_peek();
            }
        });
        let scroll = gtk::ScrolledWindow::builder()
            .child(&list)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Never)
            .max_content_height(240)
            .propagate_natural_height(true)
            .build();
        let presentation = LoadPresentation::new(&scroll, None);
        presentation.stack.set_size_request(-1, 120);
        content.append(&presentation.stack);

        let motion = gtk::EventControllerMotion::new();
        let weak_state = Rc::downgrade(self);
        motion.connect_enter(move |_, _, _| {
            if let Some(state) = weak_state.upgrade() {
                cancel_source(&state.pending_close);
            }
        });
        let weak_state = Rc::downgrade(self);
        motion.connect_leave(move |_| {
            if let Some(state) = weak_state.upgrade() {
                state.schedule_close_peek();
            }
        });
        content.add_controller(motion);

        let click = gtk::GestureClick::new();
        let weak_browser = Rc::downgrade(&self.browser);
        click.connect_released(move |_, _, _, _| {
            if let Some(browser) = weak_browser.upgrade() {
                browser.commit_peek();
            }
        });
        content.add_controller(click);

        let Some(bounds) = anchor.compute_bounds(&self.overlay) else {
            self.browser.close_peek();
            return;
        };
        content.add_css_class("peek-popover");
        let right = bounds.x() + bounds.width() + 4.0;
        let left = (bounds.x() - 260.0).max(0.0);
        let x = if right + 256.0 <= self.overlay.width() as f32 {
            right
        } else {
            left
        };
        let transition_duration = self
            .peek_behavior
            .fade_duration
            .as_millis()
            .min(u128::from(u32::MAX)) as u32;
        let revealer = gtk::Revealer::builder()
            .child(&content)
            .transition_type(gtk::RevealerTransitionType::Crossfade)
            .transition_duration(transition_duration)
            .reveal_child(false)
            .halign(gtk::Align::Start)
            .valign(gtk::Align::Start)
            .margin_start(x.round() as i32)
            .margin_top(bounds.y().round().max(0.0) as i32)
            .build();
        self.overlay.add_overlay(&revealer);
        self.peek.replace(Some(PeekView {
            revealer: revealer.clone(),
            location: location.clone(),
            presentation,
            model,
            entry_count,
            spinner,
        }));
        glib::idle_add_local_once(move || revealer.set_reveal_child(true));
    }

    fn close_peek_visual(&self) {
        cancel_source(&self.pending_peek);
        cancel_source(&self.pending_close);
        if let Some(peek) = self.peek.take() {
            peek.revealer.set_can_target(false);
            peek.revealer.set_reveal_child(false);
            let overlay = self.overlay.clone();
            let revealer = peek.revealer;
            let delay = Duration::from_millis(u64::from(revealer.transition_duration()));
            glib::timeout_add_local_once(delay, move || overlay.remove_overlay(&revealer));
        }
    }

    fn truncate(self: &Rc<Self>, len: usize) {
        self.close_peek_visual();
        self.cancel_rename();
        self.cancel_new_folder();
        self.horizontal_scroll_generation
            .set(self.horizontal_scroll_generation.get().saturating_add(1));
        while self.columns.borrow().len() > len {
            let Some(column) = self.columns.borrow_mut().pop() else {
                break;
            };
            column
                .animation_generation
                .set(column.animation_generation.get().saturating_add(1));
            self.columns_widget.remove(&column.shell);
            self.overlay.remove_overlay(&column.marquee);
        }
        let retained = self
            .columns
            .borrow()
            .last()
            .map(|column| column.shell.clone());
        if let Some(retained) = retained {
            self.reveal_column(retained);
        }
    }
}

fn basic_label_factory() -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let icon = gtk::Image::new();
        icon.add_css_class("file-icon");
        icon.set_pixel_size(17);
        let label = gtk::Label::builder()
            .halign(gtk::Align::Start)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        let chevron = crate::assets::primary_icon(crate::assets::icons::CHEVRON_RIGHT, 15);
        chevron.add_css_class("file-chevron");
        row.append(&icon);
        row.append(&label);
        row.append(&chevron);
        item.set_child(Some(&row));
    });
    factory.connect_bind(|_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let Some(value) = item.item().and_downcast::<gtk::StringObject>() else {
            return;
        };
        let Some(row) = item.child().and_downcast::<gtk::Box>() else {
            return;
        };
        let Some(icon) = row.first_child().and_downcast::<gtk::Image>() else {
            return;
        };
        let Some(label) = icon.next_sibling().and_downcast::<gtk::Label>() else {
            return;
        };
        let Some(chevron) = label.next_sibling().and_downcast::<gtk::Image>() else {
            return;
        };
        let value = value.string();
        let name = model_display_name(&value);
        let directory = model_is_directory(&value);
        label.set_label(name);
        crate::assets::set_primary_icon(
            &icon,
            if directory {
                crate::assets::icons::FOLDER
            } else {
                icon_for_name(name)
            },
        );
        icon.set_opacity(if directory { 1.0 } else { 0.72 });
        chevron.set_visible(directory);
    });
    factory
}

fn format_file_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "kB", "MB", "GB", "TB"];
    if bytes < 1_000 {
        return format!("{bytes} B");
    }

    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1_000.0 && unit < UNITS.len() - 1 {
        value /= 1_000.0;
        unit += 1;
    }
    let formatted = format!("{value:.1}");
    format!("{} {}", formatted.trim_end_matches(".0"), UNITS[unit])
}

fn set_filter_placeholder(column: &ColumnView, count: usize) {
    let noun = if count == 1 { "item" } else { "items" };
    column
        .filter_entry
        .set_placeholder_text(Some(&format!("Filter {count} {noun}…")));
}

fn source_position_for_filtered(
    source: &gtk::StringList,
    filtered: &gtk::FilterListModel,
    filtered_position: u32,
) -> Option<usize> {
    let item = filtered.item(filtered_position)?;
    (0..source.n_items())
        .find(|position| {
            source
                .item(*position)
                .is_some_and(|candidate| candidate == item)
        })
        .map(|position| position as usize)
}

fn set_column_selection(column: &ColumnView, position: u32) {
    column.syncing_selection.set(true);
    column.selection.unselect_all();
    if position != gtk::INVALID_LIST_POSITION {
        column.selection.select_item(position, true);
    }
    column.syncing_selection.set(false);
}

fn set_column_selections(column: &ColumnView, positions: &[u32]) {
    column.syncing_selection.set(true);
    column.selection.unselect_all();
    for position in positions {
        column.selection.select_item(*position, false);
    }
    column.syncing_selection.set(false);
}

fn bitset_positions(bitset: &gtk::Bitset) -> Vec<u32> {
    let Some((iterator, first)) = gtk::BitsetIter::init_first(bitset) else {
        return Vec::new();
    };
    std::iter::once(first).chain(iterator).collect()
}

fn filtered_position_for_source(column: &ColumnView, source_position: usize) -> Option<u32> {
    let item = column.model.item(source_position as u32)?;
    (0..column.filtered_model.n_items()).find(|position| {
        column
            .filtered_model
            .item(*position)
            .is_some_and(|candidate| candidate == item)
    })
}

fn install_folder_context_menu(
    state: &Rc<ViewState>,
    parent: &gtk::Stack,
    list: &gtk::ListView,
    depth: usize,
    location: Location,
) {
    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.add_css_class("folder-context-menu");
    let popover = gtk::Popover::builder()
        .child(&content)
        .autohide(true)
        .has_arrow(false)
        .build();
    popover.add_css_class("folder-context-popover");
    popover.set_parent(parent);

    let new_folder = context_menu_option("New Folder", Some("Ctrl+Shift+N"));
    let paste = context_menu_option("Paste", Some("Ctrl+V"));
    let select_all = context_menu_option("Select All", Some("Ctrl+A"));
    let properties = context_menu_option("Properties", None);
    content.append(&new_folder);
    content.append(&paste);
    content.append(&select_all);
    content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    content.append(&properties);

    let pending_new_folder = Rc::new(Cell::new(false));
    let pending_for_click = pending_new_folder.clone();
    let new_folder_popover = popover.downgrade();
    new_folder.connect_clicked(move |_| {
        pending_for_click.set(true);
        if let Some(popover) = new_folder_popover.upgrade() {
            popover.popdown();
        }
    });
    let weak = Rc::downgrade(state);
    let folder = location.clone();
    popover.connect_closed(move |_| {
        if !pending_new_folder.replace(false) {
            return;
        }
        let weak = weak.clone();
        let folder = folder.clone();
        glib::idle_add_local_once(move || {
            if let Some(state) = weak.upgrade() {
                state.begin_new_folder(depth, folder);
            }
        });
    });
    let weak = Rc::downgrade(state);
    let folder = location.clone();
    let paste_popover = popover.downgrade();
    paste.connect_clicked(move |_| {
        if let Some(popover) = paste_popover.upgrade() {
            popover.popdown();
        }
        if let Some(state) = weak.upgrade() {
            state.paste_into(folder.clone());
        }
    });
    let weak = Rc::downgrade(state);
    let select_popover = popover.downgrade();
    select_all.connect_clicked(move |_| {
        if let Some(popover) = select_popover.upgrade() {
            popover.popdown();
        }
        if let Some(state) = weak.upgrade() {
            state.select_all(depth);
        }
    });
    let weak = Rc::downgrade(state);
    let properties_popover = popover.downgrade();
    properties.connect_clicked(move |_| {
        if let Some(popover) = properties_popover.upgrade() {
            popover.popdown();
        }
        if let Some(state) = weak.upgrade() {
            state.show_folder_properties(&location);
        }
    });

    let menu_click = gtk::GestureClick::new();
    menu_click.set_button(3);
    let list = list.clone();
    let weak_popover = popover.downgrade();
    menu_click.connect_pressed(move |gesture, _, x, y| {
        let over_row = gesture
            .widget()
            .and_then(|widget| widget.pick(x, y, gtk::PickFlags::DEFAULT))
            .is_some_and(is_file_row_target);
        if over_row {
            return;
        }
        let Some(popover) = weak_popover.upgrade() else {
            return;
        };
        gesture.set_state(gtk::EventSequenceState::Claimed);
        paste.set_sensitive(gtk::gdk::Display::default().is_some_and(|display| {
            display
                .clipboard()
                .formats()
                .contains_type(gtk::gdk::FileList::static_type())
        }));
        select_all.set_sensitive(list.model().is_some_and(|model| model.n_items() > 0));
        popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(
            x.round() as i32,
            y.round() as i32,
            1,
            1,
        )));
        popover.popup();
    });
    parent.add_controller(menu_click);
}

fn install_item_context_menu(
    state: &Rc<ViewState>,
    list: &gtk::ListView,
    selection: &gtk::MultiSelection,
    bound_rows: &Rc<RefCell<Vec<BoundRow>>>,
    source: &gtk::StringList,
    filtered: &gtk::FilterListModel,
    depth: usize,
) {
    let in_trash = state
        .browser
        .location_at(depth)
        .as_ref()
        .is_some_and(is_trash_location);
    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.add_css_class("item-context-menu");
    let header = gtk::Box::new(gtk::Orientation::Vertical, 2);
    header.add_css_class("item-context-header");
    let heading = gtk::Label::new(None);
    heading.add_css_class("item-context-title");
    heading.set_ellipsize(gtk::pango::EllipsizeMode::End);
    heading.set_xalign(0.0);
    let summary = gtk::Label::new(None);
    summary.add_css_class("item-context-summary");
    summary.set_ellipsize(gtk::pango::EllipsizeMode::End);
    summary.set_xalign(0.0);
    header.append(&heading);
    header.append(&summary);
    content.append(&header);
    content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

    let single = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let open = item_context_option(crate::assets::icons::EXTERNAL_LINK, "Open", "↵");
    let preview = item_context_option(crate::assets::icons::EYE, "Quick preview", "Space");
    let pin = item_context_option(crate::assets::icons::PIN, "Pin to sidebar", "P");
    pin.set_tooltip_text(Some("Pinned locations are planned"));
    let copy_path = item_context_option(crate::assets::icons::COPY, "Copy path", "Y");
    let rename = item_context_option(crate::assets::icons::PENCIL, "Rename", "F2");
    let cut = item_context_option(crate::assets::icons::SCISSORS, "Cut", "Ctrl+X");
    let delete_label = if in_trash {
        "Permanently delete"
    } else {
        "Move to Trash"
    };
    let move_to_trash =
        item_context_danger_option(crate::assets::icons::TRASH, delete_label, "Del");
    move_to_trash.add_css_class("danger");
    let properties = item_context_option(crate::assets::icons::INFO, "Properties", "Alt+Enter");
    single.append(&open);
    single.append(&preview);
    single.append(&pin);
    single.append(&copy_path);
    single.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    single.append(&rename);
    single.append(&cut);
    single.append(&move_to_trash);
    single.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    single.append(&properties);
    content.append(&single);

    let multiple = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let copy_paths = item_context_option(crate::assets::icons::COPY, "Copy paths", "Y");
    let cut_multiple = item_context_option(crate::assets::icons::SCISSORS, "Cut", "Ctrl+X");
    let trash_multiple =
        item_context_danger_option(crate::assets::icons::TRASH, delete_label, "Del");
    trash_multiple.add_css_class("danger");
    multiple.append(&copy_paths);
    multiple.append(&cut_multiple);
    multiple.append(&trash_multiple);
    multiple.set_visible(false);
    content.append(&multiple);

    let popover = gtk::Popover::builder()
        .child(&content)
        .autohide(true)
        .has_arrow(false)
        .build();
    popover.add_css_class("folder-context-popover");
    popover.set_parent(list);

    let target = Rc::new(RefCell::new(None::<(usize, FileEntry)>));
    let weak = Rc::downgrade(state);
    let open_target = target.clone();
    let open_popover = popover.downgrade();
    open.connect_clicked(move |_| {
        if let Some(popover) = open_popover.upgrade() {
            popover.popdown();
        }
        let Some((position, _)) = open_target.borrow().clone() else {
            return;
        };
        if let Some(state) = weak.upgrade() {
            state.browser.activate(depth, position);
        }
    });
    let weak = Rc::downgrade(state);
    let preview_target = target.clone();
    let preview_popover = popover.downgrade();
    preview.connect_clicked(move |_| {
        if let Some(popover) = preview_popover.upgrade() {
            popover.popdown();
        }
        let Some((position, entry)) = preview_target.borrow().clone() else {
            return;
        };
        if let Some(state) = weak.upgrade()
            && !entry.is_directory()
        {
            state.browser.preview(depth, position);
        }
    });
    let pin_popover = popover.downgrade();
    pin.connect_clicked(move |_| {
        if let Some(popover) = pin_popover.upgrade() {
            popover.popdown();
        }
    });
    let weak = Rc::downgrade(state);
    let copy_target = target.clone();
    let copy_popover = popover.downgrade();
    copy_path.connect_clicked(move |_| {
        if let Some(popover) = copy_popover.upgrade() {
            popover.popdown();
        }
        let Some((_, entry)) = copy_target.borrow().clone() else {
            return;
        };
        if weak.upgrade().is_some() {
            copy_locations(&[entry]);
        }
    });
    let weak = Rc::downgrade(state);
    let rename_target = target.clone();
    let rename_popover = popover.downgrade();
    rename.connect_clicked(move |_| {
        if let Some(popover) = rename_popover.upgrade() {
            popover.popdown();
        }
        let Some((position, _)) = rename_target.borrow().clone() else {
            return;
        };
        let weak = weak.clone();
        glib::idle_add_local_once(move || {
            if let Some(state) = weak.upgrade() {
                state.browser.select(depth, position);
                state.begin_rename();
            }
        });
    });
    connect_context_cut(&cut, &popover, state, &target);
    connect_context_cut(&cut_multiple, &popover, state, &target);
    connect_context_trash(&move_to_trash, &popover, state, &target, in_trash);
    connect_context_trash(&trash_multiple, &popover, state, &target, in_trash);
    let weak = Rc::downgrade(state);
    let properties_target = target.clone();
    let properties_popover = popover.downgrade();
    properties.connect_clicked(move |_| {
        if let Some(popover) = properties_popover.upgrade() {
            popover.popdown();
        }
        let Some((_, entry)) = properties_target.borrow().clone() else {
            return;
        };
        if let Some(state) = weak.upgrade() {
            state.show_entry_properties(entry);
        }
    });
    let weak = Rc::downgrade(state);
    let paths_target = target.clone();
    let paths_popover = popover.downgrade();
    copy_paths.connect_clicked(move |_| {
        if let Some(popover) = paths_popover.upgrade() {
            popover.popdown();
        }
        if let Some(state) = weak.upgrade() {
            copy_locations(&context_entries(&state, &paths_target));
        }
    });

    let click = gtk::GestureClick::new();
    click.set_button(3);
    let weak_state = Rc::downgrade(state);
    let weak_popover = popover.downgrade();
    let rows = bound_rows.clone();
    let selection = selection.clone();
    let source = source.clone();
    let filtered = filtered.clone();
    click.connect_pressed(move |gesture, _, x, y| {
        let Some(picked) = gesture
            .widget()
            .and_then(|widget| widget.pick(x, y, gtk::PickFlags::DEFAULT))
            .and_then(file_row_target)
        else {
            return;
        };
        let filtered_position = rows.borrow().iter().find_map(|bound| {
            let row = bound.row.upgrade()?;
            let item = bound.item.upgrade()?;
            (row == picked).then_some(item.position())
        });
        let Some(filtered_position) = filtered_position else {
            return;
        };
        let Some(source_position) =
            source_position_for_filtered(&source, &filtered, filtered_position)
        else {
            return;
        };
        let Some(state) = weak_state.upgrade() else {
            return;
        };
        let Some(entry) = state.browser.entry_at(depth, source_position) else {
            return;
        };
        gesture.set_state(gtk::EventSequenceState::Claimed);
        if !selection.is_selected(filtered_position) {
            selection.select_item(filtered_position, true);
        }
        let selected_positions = bitset_positions(&selection.selection())
            .into_iter()
            .filter_map(|position| source_position_for_filtered(&source, &filtered, position))
            .collect::<Vec<_>>();
        state
            .browser
            .set_selection(depth, &selected_positions, Some(source_position));
        target.replace(Some((source_position, entry.clone())));
        let entries = state.browser.selected_entries();
        preview.set_sensitive(!entry.is_directory());
        if entries.len() > 1 {
            heading.set_text(&format!("{} items selected", entries.len()));
            summary.set_text(&selected_items_summary(&entries));
            single.set_visible(false);
            multiple.set_visible(true);
        } else {
            heading.set_text(&entry.display_name);
            summary.set_text(&compact_display_path(&entry.location));
            single.set_visible(true);
            multiple.set_visible(false);
        }
        let Some(popover) = weak_popover.upgrade() else {
            return;
        };
        popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(
            x.round() as i32,
            y.round() as i32,
            1,
            1,
        )));
        popover.popup();
    });
    list.add_controller(click);
}

fn selected_items_summary(entries: &[FileEntry]) -> String {
    let mut names = entries
        .iter()
        .take(3)
        .map(|entry| entry.display_name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    if entries.len() > 3 {
        names.push_str(", …");
    }
    names
}

fn context_entries(
    state: &ViewState,
    target: &RefCell<Option<(usize, FileEntry)>>,
) -> Vec<FileEntry> {
    let entries = state.browser.selected_entries();
    if entries.is_empty() {
        target
            .borrow()
            .as_ref()
            .map(|(_, entry)| vec![entry.clone()])
            .unwrap_or_default()
    } else {
        entries
    }
}

fn connect_context_trash(
    button: &gtk::Button,
    popover: &gtk::Popover,
    state: &Rc<ViewState>,
    target: &Rc<RefCell<Option<(usize, FileEntry)>>>,
    permanent: bool,
) {
    let weak = Rc::downgrade(state);
    let target = target.clone();
    let popover = popover.downgrade();
    button.connect_clicked(move |_| {
        if let Some(popover) = popover.upgrade() {
            popover.popdown();
        }
        if let Some(state) = weak.upgrade() {
            let entries = context_entries(&state, &target);
            state.show_delete_confirmation(entries, permanent);
        }
    });
}

fn connect_context_cut(
    button: &gtk::Button,
    popover: &gtk::Popover,
    state: &Rc<ViewState>,
    target: &Rc<RefCell<Option<(usize, FileEntry)>>>,
) {
    let weak = Rc::downgrade(state);
    let target = target.clone();
    let popover = popover.downgrade();
    button.connect_clicked(move |_| {
        if let Some(popover) = popover.upgrade() {
            popover.popdown();
        }
        if let Some(state) = weak.upgrade() {
            copy_files_to_clipboard(&context_entries(&state, &target));
        }
    });
}

fn install_directory_drop_target(
    state: &Rc<ViewState>,
    widget: &impl IsA<gtk::Widget>,
    destination: Location,
) {
    widget.add_css_class("file-drop-zone");
    let drop = gtk::DropTarget::new(
        gtk::gdk::FileList::static_type(),
        gtk::gdk::DragAction::COPY | gtk::gdk::DragAction::MOVE,
    );
    let weak = Rc::downgrade(state);
    drop.connect_drop(move |target, value, _, _| {
        let Some(state) = weak.upgrade() else {
            return false;
        };
        transfer_dropped_files(&state, target, value, destination.clone())
    });
    widget.add_controller(drop);
}

fn transfer_dropped_files(
    state: &Rc<ViewState>,
    target: &gtk::DropTarget,
    value: &glib::Value,
    destination: Location,
) -> bool {
    let Ok(files) = value.get::<gtk::gdk::FileList>() else {
        return false;
    };
    let sources = files
        .files()
        .iter()
        .map(location_for_gio_file)
        .collect::<Vec<_>>();
    if sources.is_empty() {
        return false;
    }
    let move_sources = target
        .current_drop()
        .and_then(|drop| drop.drag())
        .is_some_and(|drag| drag.selected_action() == gtk::gdk::DragAction::MOVE);
    state.browser.transfer(destination, sources, move_sources);
    true
}

fn location_for_gio_file(file: &gio::File) -> Location {
    file.path()
        .map(Location::local)
        .unwrap_or_else(|| Location::uri(file.uri().as_str()))
}

fn file_drag_content(entries: &[FileEntry]) -> Option<gtk::gdk::ContentProvider> {
    let files = entries
        .iter()
        .map(|entry| gio_file_for_location(&entry.location))
        .collect::<Vec<_>>();
    if files.is_empty() {
        return None;
    }
    let file_list =
        gtk::gdk::ContentProvider::for_value(&gtk::gdk::FileList::from_array(&files).to_value());
    let uri_list = files
        .iter()
        .map(|file| file.uri())
        .collect::<Vec<_>>()
        .join("\r\n")
        + "\r\n";
    let uri_list = gtk::gdk::ContentProvider::for_bytes(
        "text/uri-list",
        &glib::Bytes::from_owned(uri_list.into_bytes()),
    );
    Some(gtk::gdk::ContentProvider::new_union(&[file_list, uri_list]))
}

fn copy_locations(entries: &[FileEntry]) {
    let text = entries
        .iter()
        .map(|entry| entry.location.display_path())
        .collect::<Vec<_>>()
        .join("\n");
    if let Some(display) = gtk::gdk::Display::default() {
        display.clipboard().set_text(&text);
    }
}

fn copy_files_to_clipboard(entries: &[FileEntry]) {
    let files = entries
        .iter()
        .map(|entry| gio_file_for_location(&entry.location))
        .collect::<Vec<_>>();
    if files.is_empty() {
        return;
    }
    if let Some(display) = gtk::gdk::Display::default() {
        let _result = display
            .clipboard()
            .set_content(Some(&gtk::gdk::ContentProvider::for_value(
                &gtk::gdk::FileList::from_array(&files).to_value(),
            )));
    }
}

fn item_context_option(icon: &str, label: &str, accelerator: &str) -> gtk::Button {
    item_context_option_with_icon(crate::assets::text_icon(icon, 15), label, accelerator)
}

fn item_context_danger_option(icon: &str, label: &str, accelerator: &str) -> gtk::Button {
    item_context_option_with_icon(crate::assets::danger_icon(icon, 15), label, accelerator)
}

fn item_context_option_with_icon(icon: gtk::Image, label: &str, accelerator: &str) -> gtk::Button {
    let button = gtk::Button::new();
    button.add_css_class("item-context-option");
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    icon.add_css_class("item-context-icon");
    let title = gtk::Label::new(Some(label));
    title.set_xalign(0.0);
    title.set_hexpand(true);
    let shortcut = gtk::Label::new(Some(accelerator));
    shortcut.add_css_class("item-context-shortcut");
    row.append(&icon);
    row.append(&title);
    row.append(&shortcut);
    button.set_child(Some(&row));
    button
}

fn context_menu_option(label: &str, accelerator: Option<&str>) -> gtk::Button {
    let button = gtk::Button::new();
    button.add_css_class("folder-context-option");
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 18);
    let title = gtk::Label::new(Some(label));
    title.set_xalign(0.0);
    title.set_hexpand(true);
    row.append(&title);
    if let Some(accelerator) = accelerator {
        let shortcut = gtk::Label::new(Some(accelerator));
        shortcut.add_css_class("folder-context-shortcut");
        row.append(&shortcut);
    }
    button.set_child(Some(&row));
    button
}

fn column_sort_menu(browser: &Rc<Browser>, depth: usize) -> gtk::MenuButton {
    let content = gtk::Box::new(gtk::Orientation::Vertical, 2);
    content.add_css_class("column-menu");
    let heading = gtk::Label::new(Some("SORT BY"));
    heading.set_xalign(0.0);
    heading.add_css_class("menu-heading");
    content.append(&heading);

    let selected_checks: Rc<RefCell<Vec<gtk::Image>>> = Rc::new(RefCell::new(Vec::new()));
    for (label, key, selected) in [
        ("Name", SortKey::Name, true),
        ("Size", SortKey::Size, false),
        ("Modified", SortKey::Modified, false),
        ("Type", SortKey::Type, false),
    ] {
        let (option, check) = column_menu_option(label, selected);
        selected_checks.borrow_mut().push(check.clone());
        let index = selected_checks.borrow().len() - 1;
        let checks = selected_checks.clone();
        let weak_browser = Rc::downgrade(browser);
        option.connect_clicked(move |_| {
            for (check_index, check) in checks.borrow().iter().enumerate() {
                check.set_visible(check_index == index);
            }
            if let Some(browser) = weak_browser.upgrade() {
                browser.set_sort_key(depth, key);
            }
        });
        content.append(&option);
    }

    content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    let (folders_first, folders_check) = column_menu_option("Folders first", true);
    let folders_enabled = Rc::new(Cell::new(true));
    let weak_browser = Rc::downgrade(browser);
    folders_first.connect_clicked(move |_| {
        let enabled = !folders_enabled.get();
        folders_enabled.set(enabled);
        folders_check.set_visible(enabled);
        if let Some(browser) = weak_browser.upgrade() {
            browser.set_folders_first(depth, enabled);
        }
    });
    content.append(&folders_first);

    let popover = gtk::Popover::builder()
        .child(&content)
        .has_arrow(false)
        .halign(gtk::Align::End)
        .position(gtk::PositionType::Bottom)
        .build();
    popover.add_css_class("column-popover");
    let button = gtk::MenuButton::builder()
        .tooltip_text("Choose sort field")
        .popover(&popover)
        .build();
    button.set_child(Some(&crate::assets::text_icon(
        crate::assets::icons::SETTINGS_2,
        16,
    )));
    button.add_css_class("column-header-action");
    button
}

fn column_sort_direction_toggle(browser: &Rc<Browser>, depth: usize) -> gtk::ToggleButton {
    let button = gtk::ToggleButton::builder()
        .tooltip_text("Ascending — click to reverse")
        .build();
    let icon = crate::assets::text_icon(crate::assets::icons::ARROW_UP_NARROW_WIDE, 16);
    button.set_child(Some(&icon));
    button.add_css_class("column-header-action");
    let weak_browser = Rc::downgrade(browser);
    button.connect_toggled(move |button| {
        let direction = if button.is_active() {
            crate::assets::set_text_icon(&icon, crate::assets::icons::ARROW_DOWN_WIDE_NARROW);
            button.set_tooltip_text(Some("Descending — click to reverse"));
            SortDirection::Descending
        } else {
            crate::assets::set_text_icon(&icon, crate::assets::icons::ARROW_UP_NARROW_WIDE);
            button.set_tooltip_text(Some("Ascending — click to reverse"));
            SortDirection::Ascending
        };
        if let Some(browser) = weak_browser.upgrade() {
            browser.set_sort_direction(depth, direction);
        }
    });
    button
}

fn column_menu_option(label: &str, selected: bool) -> (gtk::Button, gtk::Image) {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    let check = crate::assets::primary_icon(crate::assets::icons::CHECK, 16);
    check.set_visible(selected);
    let label = gtk::Label::new(Some(label));
    label.set_xalign(0.0);
    label.set_hexpand(true);
    row.append(&label);
    row.append(&check);
    let option = gtk::Button::builder().child(&row).build();
    option.add_css_class("column-menu-option");
    option.set_has_frame(false);
    (option, check)
}

fn file_row_target(mut target: gtk::Widget) -> Option<gtk::Box> {
    loop {
        if target.has_css_class("file-row") {
            return target.downcast::<gtk::Box>().ok();
        }
        if target.is::<gtk::ListView>() {
            return None;
        }
        target = target.parent()?;
    }
}

fn is_file_row_target(target: gtk::Widget) -> bool {
    file_row_target(target).is_some()
}

fn is_breadcrumb_target(mut target: gtk::Widget) -> bool {
    loop {
        if target.is::<gtk::Button>()
            || target.has_css_class("breadcrumb")
            || target.has_css_class("breadcrumb-separator")
            || target.has_css_class("current-breadcrumb")
        {
            return true;
        }
        let Some(parent) = target.parent() else {
            return false;
        };
        if parent.has_css_class("breadcrumbs") {
            return false;
        }
        target = parent;
    }
}

fn set_active_path_style(row: &gtk::Box, active: bool) {
    if active {
        row.add_css_class("active-path");
    } else {
        row.remove_css_class("active-path");
    }
}

fn rename_stem_end(name: &str) -> i32 {
    let end = name
        .rfind('.')
        .filter(|position| *position > 0)
        .unwrap_or(name.len());
    name[..end].chars().count().min(i32::MAX as usize) as i32
}

fn entry_model_value(entry: &FileEntry) -> String {
    let kind = if entry.is_broken_symbolic_link() {
        'x'
    } else if entry.is_directory() {
        'd'
    } else if entry.is_symbolic_link() {
        's'
    } else {
        'f'
    };
    format!("{kind}\t{}", entry.display_name)
}

fn model_display_name(value: &str) -> &str {
    value.split_once('\t').map_or(value, |(_, name)| name)
}

fn model_is_directory(value: &str) -> bool {
    value.starts_with("d\t")
}

fn entry_icon(entry: &FileEntry) -> &'static str {
    if entry.is_broken_symbolic_link() {
        return crate::assets::icons::X;
    }
    if entry.is_directory() {
        return crate::assets::icons::FOLDER;
    }
    icon_for_name(&entry.display_name)
}

fn icon_for_name(name: &str) -> &'static str {
    let extension = name
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase());
    match extension.as_deref() {
        Some("sh" | "bash" | "zsh" | "fish") => crate::assets::icons::TERMINAL,
        Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "bmp" | "avif") => {
            crate::assets::icons::PICTURES
        }
        Some("mp4" | "mkv" | "webm" | "mov" | "avi" | "m4v") => crate::assets::icons::VIDEOS,
        Some("zip" | "tar" | "gz" | "bz2" | "xz" | "7z" | "rar" | "zst") => {
            crate::assets::icons::FILE_ARCHIVE
        }
        Some(
            "rs" | "c" | "h" | "cpp" | "go" | "py" | "rb" | "java" | "js" | "jsx" | "ts" | "tsx"
            | "lua" | "php" | "html" | "css" | "scss" | "json",
        ) => crate::assets::icons::FILE_CODE,
        _ => crate::assets::icons::DOCUMENTS,
    }
}

fn append_entries(
    model: &gtk::StringList,
    stored_count: &Rc<Cell<usize>>,
    entries: Vec<FileEntry>,
    limit: Option<usize>,
) {
    let remaining = limit
        .map(|limit| limit.max(1).saturating_sub(stored_count.get()))
        .unwrap_or(entries.len());
    let mut appended = 0;
    for entry in entries.into_iter().take(remaining) {
        model.append(&entry_model_value(&entry));
        appended += 1;
    }
    stored_count.set(stored_count.get() + appended);
}

fn cancel_source(source: &RefCell<Option<glib::SourceId>>) {
    if let Some(source) = source.take() {
        source.remove();
    }
}

fn animate_column_entry(shell: &gtk::Box, column: &gtk::Box, generation: &Rc<Cell<u64>>) {
    let animation_id = generation.get().saturating_add(1);
    generation.set(animation_id);
    if !animations_enabled() {
        column.set_opacity(1.0);
        column.set_margin_start(0);
        return;
    }

    column.set_opacity(0.0);
    column.set_margin_start(COLUMN_OFFSET);
    let started = Instant::now();
    let shell = shell.clone();
    let column = column.clone();
    let generation = generation.clone();
    let _tick = shell.add_tick_callback(move |_, _| {
        if generation.get() != animation_id {
            return glib::ControlFlow::Break;
        }
        let progress =
            (started.elapsed().as_secs_f64() / COLUMN_TRANSITION.as_secs_f64()).clamp(0.0, 1.0);
        let eased = emphasized_deceleration(progress);
        column.set_opacity(eased);
        column.set_margin_start((f64::from(COLUMN_OFFSET) * (1.0 - eased)).round() as i32);
        if progress >= 1.0 {
            column.set_opacity(1.0);
            column.set_margin_start(0);
            glib::ControlFlow::Break
        } else {
            glib::ControlFlow::Continue
        }
    });
}

fn resized_column_width(initial_width: i32, horizontal_offset: f64) -> i32 {
    (f64::from(initial_width) + horizontal_offset)
        .round()
        .max(f64::from(COLUMN_WIDTH)) as i32
}

fn horizontal_reveal_target(
    current: f64,
    page_size: f64,
    lower: f64,
    upper: f64,
    item_left: f64,
    item_right: f64,
) -> f64 {
    let viewport_right = current + page_size;
    let target = if item_right > viewport_right {
        item_right - page_size
    } else if item_left < current {
        item_left
    } else {
        current
    };
    target.clamp(lower, (upper - page_size).max(lower))
}

fn animate_horizontal_scroll(
    scroller: &gtk::ScrolledWindow,
    adjustment: &gtk::Adjustment,
    target: f64,
    generation: &Rc<Cell<u64>>,
    animation_id: u64,
) {
    let start = adjustment.value();
    if !animations_enabled() || (target - start).abs() < 0.5 {
        adjustment.set_value(target);
        return;
    }

    let started = Instant::now();
    let adjustment = adjustment.clone();
    let generation = generation.clone();
    let _tick = scroller.add_tick_callback(move |_, _| {
        if generation.get() != animation_id {
            return glib::ControlFlow::Break;
        }
        let progress =
            (started.elapsed().as_secs_f64() / COLUMN_TRANSITION.as_secs_f64()).clamp(0.0, 1.0);
        let eased = emphasized_deceleration(progress);
        adjustment.set_value(start + (target - start) * eased);
        if progress >= 1.0 {
            adjustment.set_value(target);
            glib::ControlFlow::Break
        } else {
            glib::ControlFlow::Continue
        }
    });
}

fn item_count_label(count: usize) -> String {
    if count == 1 {
        "1 item".to_owned()
    } else {
        format!("{count} items")
    }
}

fn entry_kind_summary(entries: &[FileEntry]) -> String {
    let directories = entries.iter().filter(|entry| entry.is_directory()).count();
    let files = entries.len().saturating_sub(directories);
    match (files, directories) {
        (1, 0) => "1 file".to_owned(),
        (files, 0) => format!("{files} files"),
        (0, 1) => "1 folder".to_owned(),
        (0, directories) => format!("{directories} folders"),
        _ => item_count_label(entries.len()),
    }
}

fn modal_layer(content: &impl IsA<gtk::Widget>) -> gtk::Box {
    let layer = gtk::Box::new(gtk::Orientation::Vertical, 0);
    layer.add_css_class("app-modal-layer");
    layer.add_css_class("modal-backdrop");
    layer.set_halign(gtk::Align::Fill);
    layer.set_valign(gtk::Align::Fill);
    layer.set_hexpand(true);
    layer.set_vexpand(true);
    layer.set_focusable(true);
    let top = gtk::Box::new(gtk::Orientation::Vertical, 0);
    top.set_vexpand(true);
    let bottom = gtk::Box::new(gtk::Orientation::Vertical, 0);
    bottom.set_vexpand(true);
    layer.append(&top);
    layer.append(content);
    layer.append(&bottom);
    layer
}

fn dismiss_modal_layer(layer: &gtk::Box, overlay: &gtk::Overlay, root: Option<&BlurBin>) {
    overlay.remove_overlay(layer);
    if let Some(root) = root {
        root.set_blurred(false);
    }
}

fn gio_file_for_location(location: &Location) -> gio::File {
    location
        .native_path()
        .map(gio::File::for_path)
        .unwrap_or_else(|| gio::File::for_uri(location.uri_value().unwrap_or_default()))
}

fn is_trash_location(location: &Location) -> bool {
    location
        .uri_value()
        .is_some_and(|uri| uri.starts_with("trash:"))
}

fn compact_display_path(location: &Location) -> String {
    let Some(path) = location.native_path() else {
        return location.display_path();
    };
    let home = glib::home_dir();
    if path == home {
        return "~".to_owned();
    }
    path.strip_prefix(&home)
        .ok()
        .map(|suffix| format!("~/{}", suffix.to_string_lossy()))
        .unwrap_or_else(|| location.display_path())
}

fn metadata_modified(entry: &FileEntry) -> String {
    let crate::model::MetadataValue::Known(seconds) = entry.modified_unix_seconds else {
        return "—".to_owned();
    };
    glib::DateTime::from_unix_local(seconds)
        .and_then(|date| date.format("%Y-%m-%d %H:%M"))
        .map(|value| value.to_string())
        .unwrap_or_else(|_| "—".to_owned())
}

fn properties_row(parent: &gtk::Box, label: &str, value: &str) -> gtk::Label {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    row.add_css_class("properties-row");
    let label = gtk::Label::new(Some(label));
    label.add_css_class("properties-row-label");
    label.set_xalign(0.0);
    let value = gtk::Label::new(Some(value));
    value.add_css_class("properties-row-value");
    value.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    value.set_hexpand(true);
    value.set_xalign(0.0);
    row.append(&label);
    row.append(&value);
    parent.append(&row);
    value
}

type PermissionRow = (gtk::Label, [gtk::Label; 3]);

fn permission_row(parent: &gtk::Box, label: &str) -> PermissionRow {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    row.add_css_class("properties-permission-row");
    let title = gtk::Label::new(Some(label));
    title.add_css_class("properties-permission-title");
    title.set_xalign(0.0);
    let identity = gtk::Label::new(Some("—"));
    identity.add_css_class("properties-permission-identity");
    identity.set_xalign(0.0);
    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    let read = gtk::Label::new(Some("—"));
    let write = gtk::Label::new(Some("—"));
    let execute = gtk::Label::new(Some("—"));
    for permission in [&read, &write, &execute] {
        permission.add_css_class("properties-permission-bit");
        permission.set_width_chars(2);
    }
    row.append(&title);
    row.append(&identity);
    row.append(&spacer);
    row.append(&read);
    row.append(&write);
    row.append(&execute);
    parent.append(&row);
    (identity, [read, write, execute])
}

fn set_permission_row(row: &PermissionRow, mode: u32, shift: u32) {
    let value = (mode >> shift) & 0o7;
    row.1[0].set_text(if value & 0o4 != 0 { "r" } else { "—" });
    row.1[1].set_text(if value & 0o2 != 0 { "w" } else { "—" });
    row.1[2].set_text(if value & 0o1 != 0 { "x" } else { "—" });
    for (index, permission) in row.1.iter().enumerate() {
        let enabled = value & [0o4, 0o2, 0o1][index] != 0;
        if enabled {
            permission.add_css_class("enabled");
        } else {
            permission.remove_css_class("enabled");
        }
    }
}

fn format_permissions(mode: u32) -> String {
    let kind = if mode & 0o170000 == 0o040000 {
        'd'
    } else {
        '-'
    };
    let mut symbolic = String::with_capacity(10);
    symbolic.push(kind);
    for (mask, character) in [
        (0o400, 'r'),
        (0o200, 'w'),
        (0o100, 'x'),
        (0o040, 'r'),
        (0o020, 'w'),
        (0o010, 'x'),
        (0o004, 'r'),
        (0o002, 'w'),
        (0o001, 'x'),
    ] {
        symbolic.push(if mode & mask != 0 { character } else { '-' });
    }
    format!("{symbolic}  {:03o}", mode & 0o777)
}

fn properties_action(icon: &str, label: &str) -> gtk::Button {
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    content.append(&crate::assets::primary_icon(icon, 14));
    content.append(&gtk::Label::new(Some(label)));
    let button = gtk::Button::builder().child(&content).build();
    button.add_css_class("properties-action");
    button
}

fn open_location(location: &Location, parent: &impl IsA<gtk::Widget>) {
    let file = gio_file_for_location(location);
    let uri = file.uri();
    if let Err(error) = gio::AppInfo::launch_default_for_uri(&uri, None::<&gio::AppLaunchContext>) {
        tracing::warn!(location = %location.display_path(), error = %error, "unable to open file");
        show_error_dialog(parent, "Unable to open file", &error.to_string());
    }
}

fn show_error_dialog(parent: &impl IsA<gtk::Widget>, message: &str, detail: &str) {
    let dialog = gtk::AlertDialog::builder()
        .modal(true)
        .message(message)
        .detail(detail)
        .build();
    let window = parent.root().and_downcast::<gtk::Window>();
    dialog.show(window.as_ref());
}

#[cfg(test)]
mod tests;
