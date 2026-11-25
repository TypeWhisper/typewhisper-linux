//! Terminals pass Ctrl+V on to the program running in them, so TypeWhisper
//! pastes into them with Ctrl+Shift+V. A terminal is recognized by its AT-SPI
//! role, as VTE terminals such as GNOME Terminal and Ptyxis show it, or by the
//! app ID the desktop reports: from a built-in list or from the terminals the
//! user adds in the settings, as on macOS.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};
use tw_platform::i18n::{n_, tr, tr_with};

/// App IDs of terminals that paste with Ctrl+Shift+V, in lower case. Desktops
/// report the ID of the app's desktop file, or the window class of a window
/// that has none.
const BUILT_IN: &[&str] = &[
    "alacritty",
    "com.gexperts.tilix",
    "com.mitchellh.ghostty",
    "com.raggesilver.blackbox",
    "com.system76.cosmicterm",
    "foot",
    "footclient",
    "guake",
    "io.elementary.terminal",
    "kitty",
    "lxterminal",
    "mate-terminal",
    "org.codeberg.dnkl.foot",
    "org.codeberg.dnkl.footclient",
    "org.gnome.console",
    "org.gnome.ptyxis",
    "org.gnome.terminal",
    "org.kde.konsole",
    "org.kde.yakuake",
    "org.wezfurlong.wezterm",
    "qterminal",
    "terminator",
    "xfce4-terminal",
];

/// The description of the settings group.
pub const DESCRIPTION: &str = n_(
    "TypeWhisper pastes dictations into terminal apps with Ctrl+Shift+V, \
     because terminals pass Ctrl+V on to the program running in them. GNOME Terminal, Ptyxis, \
     Console, Konsole, Alacritty, kitty, WezTerm, foot, Ghostty and other well-known terminals \
     are always included. Add another terminal if dictating into it pastes nothing.",
);

/// Lower case and without `.desktop`, as in the lists.
fn normalized(app_id: &str) -> String {
    let id = app_id.trim().to_lowercase();
    match id.strip_suffix(".desktop") {
        Some(stripped) => stripped.to_owned(),
        None => id,
    }
}

/// True when `app_id` is a built-in terminal or one of `added`.
pub fn is_terminal(app_id: &str, added: &[String]) -> bool {
    let id = normalized(app_id);
    !id.is_empty()
        && (BUILT_IN.contains(&id.as_str()) || added.iter().any(|other| normalized(other) == id))
}

/// Receives the whole list after every change.
type Save = Box<dyn Fn(&[String])>;

/// The "Terminal Apps" settings group: the terminals the user added, each with
/// a button that removes it, and a button that opens the app picker.
pub struct TerminalApps {
    group: adw::PreferencesGroup,
    select: adw::ButtonRow,
    rows: RefCell<Vec<adw::ActionRow>>,
    app_ids: RefCell<Vec<String>>,
    save: Save,
}

impl TerminalApps {
    /// Fills `group` with `app_ids`. `save` gets the whole list after every change.
    pub fn new(
        group: &adw::PreferencesGroup,
        app_ids: Vec<String>,
        save: impl Fn(&[String]) + 'static,
    ) -> Rc<Self> {
        let select = adw::ButtonRow::builder().title(tr("Select Apps…")).build();
        group.add(&select);
        let this = Rc::new(Self {
            group: group.clone(),
            select,
            rows: RefCell::default(),
            app_ids: RefCell::new(app_ids),
            save: Box::new(save),
        });
        this.show_rows();
        let weak = Rc::downgrade(&this);
        this.select.connect_activated(move |row| {
            if let Some(this) = weak.upgrade() {
                this.pick(row);
            }
        });
        this
    }

    fn contains(&self, app_id: &str) -> bool {
        self.app_ids.borrow().iter().any(|id| id == app_id)
    }

    /// Adds `app_id`, or removes it when it is in the list, and saves the list.
    fn toggle(self: &Rc<Self>, app_id: &str) {
        {
            let mut app_ids = self.app_ids.borrow_mut();
            match app_ids.iter().position(|id| id == app_id) {
                Some(index) => {
                    app_ids.remove(index);
                }
                None => app_ids.push(app_id.to_owned()),
            }
            (self.save)(&app_ids);
        }
        self.show_rows();
    }

    fn show_rows(self: &Rc<Self>) {
        for row in self.rows.take() {
            self.group.remove(&row);
        }
        // The rows go above the button.
        self.group.remove(&self.select);
        let mut rows = Vec::new();
        for app_id in self.app_ids.borrow().iter() {
            let (name, icon) = describe(app_id);
            let row = adw::ActionRow::builder()
                .title(name.as_deref().unwrap_or(app_id))
                .subtitle(if name.is_some() { app_id.as_str() } else { "" })
                .use_markup(false)
                .build();
            row.add_prefix(&app_icon(icon.as_ref()));
            let remove = gtk::Button::builder()
                .icon_name("list-remove-symbolic")
                .tooltip_text(tr("Remove"))
                .valign(gtk::Align::Center)
                .css_classes(["flat"])
                .build();
            remove.update_property(&[gtk::accessible::Property::Label(&tr_with(
                "Remove {app}",
                &[("app", name.as_deref().unwrap_or(app_id))],
            ))]);
            let weak = Rc::downgrade(self);
            let id = app_id.clone();
            remove.connect_clicked(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.toggle(&id);
                }
            });
            row.add_suffix(&remove);
            self.group.add(&row);
            rows.push(row);
        }
        self.group.add(&self.select);
        *self.rows.borrow_mut() = rows;
    }

    /// The installed apps with a check box each. A change applies right away.
    /// A search that finds no app offers the text as an app ID, for apps this
    /// list does not show, such as those outside the Flatpak sandbox.
    fn pick(self: &Rc<Self>, parent: &impl IsA<gtk::Widget>) {
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .valign(gtk::Align::Start)
            .build();
        for (app_id, name, icon) in installed_apps() {
            let row = adw::ActionRow::builder()
                .title(&name)
                .subtitle(&app_id)
                .use_markup(false)
                .build();
            row.add_prefix(&app_icon(icon.as_ref()));
            let check = gtk::CheckButton::builder()
                .active(self.contains(&app_id))
                .valign(gtk::Align::Center)
                .css_classes(["selection-mode"])
                .build();
            row.add_suffix(&check);
            row.set_activatable_widget(Some(&check));
            let weak = Rc::downgrade(self);
            check.connect_toggled(move |check| {
                if let Some(this) = weak.upgrade()
                    && this.contains(&app_id) != check.is_active()
                {
                    this.toggle(&app_id);
                }
            });
            list.append(&row);
        }

        let search = gtk::SearchEntry::builder()
            .placeholder_text(tr("Search Apps…"))
            .build();
        let none = gtk::Label::builder()
            .label(tr("No apps found"))
            .css_classes(["dim-label"])
            .build();
        let add = gtk::Button::builder()
            .halign(gtk::Align::Center)
            .css_classes(["pill"])
            .visible(false)
            .build();
        let placeholder = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(24)
            .margin_bottom(24)
            .margin_start(12)
            .margin_end(12)
            .build();
        placeholder.append(&none);
        placeholder.append(&add);
        list.set_placeholder(Some(&placeholder));
        list.set_filter_func(glib::clone!(
            #[weak]
            search,
            #[upgrade_or]
            true,
            move |row| matches(row, search.text().trim())
        ));
        search.connect_search_changed(glib::clone!(
            #[weak]
            list,
            #[weak]
            add,
            move |search| {
                let query = search.text().trim().to_owned();
                add.set_label(&tr_with("Add “{app_id}” as App ID", &[("app_id", &query)]));
                add.set_visible(!query.is_empty());
                list.invalidate_filter();
            }
        ));
        let weak = Rc::downgrade(self);
        add.connect_clicked(glib::clone!(
            #[weak]
            search,
            move |_| {
                let query = search.text().trim().to_owned();
                if let Some(this) = weak.upgrade()
                    && !query.is_empty()
                    && !this.contains(&query)
                {
                    this.toggle(&query);
                }
                search.set_text("");
            }
        ));

        let done = gtk::Button::builder()
            .label(tr("Done"))
            .css_classes(["suggested-action"])
            .build();
        let header = adw::HeaderBar::builder()
            .show_start_title_buttons(false)
            .show_end_title_buttons(false)
            .build();
        header.pack_end(&done);
        let search_bar = adw::Clamp::builder()
            .margin_start(12)
            .margin_end(12)
            .margin_bottom(6)
            .child(&search)
            .build();
        let scrolled = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(
                &adw::Clamp::builder()
                    .margin_top(6)
                    .margin_bottom(12)
                    .margin_start(12)
                    .margin_end(12)
                    .child(&list)
                    .build(),
            )
            .build();
        let view = adw::ToolbarView::new();
        view.add_top_bar(&header);
        view.add_top_bar(&search_bar);
        view.set_content(Some(&scrolled));
        let dialog = adw::Dialog::builder()
            .title(tr("Select Apps"))
            .content_width(440)
            .content_height(560)
            .child(&view)
            .build();
        search.set_key_capture_widget(Some(&dialog));
        done.connect_clicked(glib::clone!(
            #[weak]
            dialog,
            move |_| {
                dialog.close();
            }
        ));
        dialog.present(Some(parent));
        search.grab_focus();
    }
}

/// True when the row's name or app ID contains `query`, ignoring case.
fn matches(row: &gtk::ListBoxRow, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let Some(row) = row.downcast_ref::<adw::ActionRow>() else {
        return true;
    };
    let query = query.to_lowercase();
    row.title().to_lowercase().contains(&query)
        || row
            .subtitle()
            .is_some_and(|s| s.to_lowercase().contains(&query))
}

/// The apps that show in menus, sorted by name: app ID, name and icon.
fn installed_apps() -> Vec<(String, String, Option<gio::Icon>)> {
    let mut apps: Vec<_> = gio::AppInfo::all()
        .into_iter()
        .filter(|app| app.should_show())
        .filter_map(|app| {
            let id = app.id()?;
            let id = id.strip_suffix(".desktop").unwrap_or(&id).to_owned();
            Some((id, app.display_name().to_string(), app.icon()))
        })
        .collect();
    apps.sort_by_cached_key(|(id, name, _)| (name.to_lowercase(), id.clone()));
    apps.dedup_by(|a, b| a.0 == b.0);
    apps
}

/// The name and icon of an installed app, from its desktop file.
fn describe(app_id: &str) -> (Option<String>, Option<gio::Icon>) {
    let desktop_id = format!("{app_id}.desktop");
    gio::AppInfo::all()
        .into_iter()
        .find(|app| app.id().is_some_and(|id| id == desktop_id))
        .map_or((None, None), |app| {
            (Some(app.display_name().to_string()), app.icon())
        })
}

fn app_icon(icon: Option<&gio::Icon>) -> gtk::Image {
    let image = match icon {
        Some(icon) => gtk::Image::from_gicon(icon),
        None => gtk::Image::from_icon_name("application-x-executable-symbolic"),
    };
    image.set_pixel_size(32);
    image.add_css_class("lowres-icon");
    image
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_built_in_terminals_as_desktops_report_them() {
        // GNOME Shell reports desktop file IDs, KWin reports them without `.desktop`.
        assert!(is_terminal("org.gnome.Ptyxis.desktop", &[]));
        assert!(is_terminal("org.kde.konsole", &[]));
        // A window without a desktop file shows its window class.
        assert!(is_terminal("Alacritty", &[]));
        assert!(!is_terminal("org.gnome.TextEditor.desktop", &[]));
        assert!(!is_terminal("", &[]));
    }

    #[test]
    fn recognizes_terminals_the_user_added() {
        let added = vec!["com.example.Term".to_owned()];
        assert!(is_terminal("com.example.Term.desktop", &added));
        assert!(is_terminal("COM.EXAMPLE.TERM", &added));
        assert!(!is_terminal("com.example.Editor", &added));
    }

    #[test]
    fn built_in_list_is_lower_case_and_sorted() {
        assert!(BUILT_IN.iter().all(|id| id.to_lowercase() == *id));
        assert!(BUILT_IN.windows(2).all(|pair| pair[0] < pair[1]));
    }
}
