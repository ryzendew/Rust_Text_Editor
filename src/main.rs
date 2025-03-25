mod text_buffer;

use anyhow::Result;
use std::sync::{Arc, Mutex};
use log::{info, error, debug, warn};
use gtk::prelude::*;
use gtk::{TextBuffer, TextTag, TextTagTable};
use gtk::glib;
use std::env;
use std::fs;
use text_buffer::TextBuffer as EditorBuffer;
use pangocairo;
use pango;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;
use std::ops::Range;
use gtk::{ApplicationWindow, TextView, Button, Box as GtkBox, Label, Entry};
use gtk::gdk::Key;
use gtk::gdk::Display;
use gtk::gio::SimpleAction;

struct RecentFilesManager {
    recent_files: Vec<PathBuf>,
    max_files: usize,
}

impl RecentFilesManager {
    fn new(max_files: usize) -> Self {
        Self {
            recent_files: Vec::new(),
            max_files,
        }
    }

    fn add_file(&mut self, path: PathBuf) {
        // Remove if already exists
        self.recent_files.retain(|p| p != &path);
        
        // Add to front
        self.recent_files.insert(0, path);
        
        // Trim if too many
        if self.recent_files.len() > self.max_files {
            self.recent_files.truncate(self.max_files);
        }
    }
    
    fn get_recent_files(&self) -> &[PathBuf] {
        &self.recent_files
    }
}

struct EditorState {
    current_file: Option<PathBuf>,
    is_modified: bool,
    text_buffer: EditorBuffer,
    selection_start: Option<usize>,
    selection_end: Option<usize>,
    zoom_level: f64,
    recent_files: RecentFilesManager,
    tab_name: String,
    active_tab_id: usize,
    undo_stack: Vec<String>,
    redo_stack: Vec<String>,
    last_saved_text: Option<String>,
    timeout_id: Option<glib::SourceId>,
}

impl EditorState {
    fn new() -> Self {
        Self {
            current_file: None,
            is_modified: false,
            text_buffer: EditorBuffer::new(),
            selection_start: None,
            selection_end: None,
            zoom_level: 1.0,
            recent_files: RecentFilesManager::new(10),
            tab_name: "Untitled".to_string(),
            active_tab_id: 0,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            last_saved_text: None,
            timeout_id: None,
        }
    }

    fn open_file(&mut self, path: &PathBuf) -> Result<String> {
        let content = fs::read_to_string(path)?;
        self.current_file = Some(path.clone());
        self.is_modified = false;
        self.text_buffer.set_text(&content);
        self.recent_files.add_file(path.clone());
        self.update_tab_name();
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.mark_saved();
        Ok(content)
    }

    fn save_file(&mut self, path: &PathBuf) -> Result<()> {
        fs::write(path, self.text_buffer.text())?;
        self.current_file = Some(path.clone());
        self.is_modified = false;
        self.recent_files.add_file(path.clone());
        self.update_tab_name();
        self.mark_saved();
        Ok(())
    }

    fn insert_text(&mut self, text: &str) {
        self.text_buffer.insert(text);
        self.is_modified = true;
    }

    fn delete_backward(&mut self) {
        self.text_buffer.delete_backward();
        self.is_modified = true;
    }

    fn delete_forward(&mut self) {
        self.text_buffer.delete_forward();
        self.is_modified = true;
    }

    fn get_cursor_position(&self) -> usize {
        self.text_buffer.cursor_position()
    }

    fn get_line_number(&self) -> usize {
        self.text_buffer.line_at_offset(self.text_buffer.cursor_position()) + 1
    }

    fn select_word_at_cursor(&mut self) {
        let range = self.text_buffer.get_word_boundary_at_offset(self.text_buffer.cursor_position());
        self.selection_start = Some(range.start);
        self.selection_end = Some(range.end);
        self.text_buffer.set_selection(Some(range));
    }

    fn update_selection(&mut self, start: usize, end: usize) {
        self.selection_start = Some(start);
        self.selection_end = Some(end);
        self.text_buffer.set_selection(Some(start..end));
    }

    fn clear_selection(&mut self) {
        self.selection_start = None;
        self.selection_end = None;
        self.text_buffer.set_selection(None);
    }

    fn get_cursor_line(&self) -> usize {
        self.text_buffer.line_at_offset(self.text_buffer.cursor_position()) + 1
    }

    fn get_cursor_column(&self) -> usize {
        self.text_buffer.column_at_offset(self.text_buffer.cursor_position()) + 1
    }

    fn zoom_in(&mut self) {
        if self.zoom_level < 3.0 {
            self.zoom_level += 0.1;
        }
    }
    
    fn zoom_out(&mut self) {
        if self.zoom_level > 0.5 {
            self.zoom_level -= 0.1;
        }
    }
    
    fn reset_zoom(&mut self) {
        self.zoom_level = 1.0;
    }

    fn update_tab_name(&mut self) {
        if let Some(path) = &self.current_file {
            if let Some(file_name) = path.file_name() {
                self.tab_name = file_name.to_string_lossy().to_string();
            }
        } else {
            self.tab_name = "Untitled".to_string();
        }
    }

    fn push_to_undo_stack(&mut self, text: &str) {
        self.undo_stack.push(text.to_string());
        if self.undo_stack.len() > 100 {
            // Limit the size of the undo stack
            self.undo_stack.remove(0);
        }
        // Clear redo stack when new changes are made
        self.redo_stack.clear();
    }

    fn undo(&mut self) -> Option<String> {
        if let Some(current_text) = self.undo_stack.pop() {
            let previous_text = if self.undo_stack.is_empty() {
                String::new()
            } else {
                self.undo_stack.last().unwrap().clone()
            };
            self.redo_stack.push(current_text);
            Some(previous_text)
        } else {
            None
        }
    }

    fn redo(&mut self) -> Option<String> {
        if let Some(next_text) = self.redo_stack.pop() {
            self.undo_stack.push(next_text.clone());
            Some(next_text)
        } else {
            None
        }
    }

    fn is_modified_from_last_save(&self) -> bool {
        if let Some(last_saved) = &self.last_saved_text {
            last_saved != self.text_buffer.text()
        } else {
            self.text_buffer.text().len() > 0
        }
    }

    fn mark_saved(&mut self) {
        self.is_modified = false;
        self.last_saved_text = Some(self.text_buffer.text().to_string());
    }
}

// Define a TabInfo struct to track tab data
struct TabInfo {
    id: usize,
    name: String,
    buffer: gtk::TextBuffer,
    file_path: Option<PathBuf>,
    is_modified: bool,
}

impl TabInfo {
    fn new(id: usize, buffer: gtk::TextBuffer) -> Self {
        Self {
            id,
            name: format!("Untitled {}", id),
            buffer,
            file_path: None,
            is_modified: false,
        }
    }
    
    fn update_name(&mut self) {
        if let Some(path) = &self.file_path {
            if let Some(file_name) = path.file_name() {
                self.name = file_name.to_string_lossy().to_string();
            }
        } else {
            self.name = format!("Untitled {}", self.id);
        }
    }
}

fn create_tag_table() -> TextTagTable {
    let tag_table = TextTagTable::new();
    
    // Create syntax highlighting tags with dark mode friendly colors
    let keyword_tag = TextTag::builder()
        .name("keyword")
        .foreground("#569CD6")  // Light blue for keywords
        .build();
    
    let function_tag = TextTag::builder()
        .name("function")
        .foreground("#DCDCAA")  // Light yellow for functions
        .build();
    
    let type_tag = TextTag::builder()
        .name("type")
        .foreground("#4EC9B0")  // Teal for types
        .build();
    
    let string_tag = TextTag::builder()
        .name("string")
        .foreground("#CE9178")  // Rust/brown for strings
        .build();
    
    let number_tag = TextTag::builder()
        .name("number")
        .foreground("#B5CEA8")  // Light green for numbers
        .build();
    
    let comment_tag = TextTag::builder()
        .name("comment")
        .foreground("#6A9955")  // Green for comments
        .build();
    
    let error_tag = TextTag::builder()
        .name("error")
        .foreground("#F44747")  // Bright red for errors
        .underline(pango::Underline::Error)
        .build();
    
    // Add tags to the table
    tag_table.add(&keyword_tag);
    tag_table.add(&function_tag);
    tag_table.add(&type_tag);
    tag_table.add(&string_tag);
    tag_table.add(&number_tag);
    tag_table.add(&comment_tag);
    tag_table.add(&error_tag);
    
    tag_table
}

fn create_tab_transition<W: IsA<gtk::Widget>>(widget: &W) {
    let provider = gtk::CssProvider::new();
    provider.load_from_data(
        "
        .tab-transition {
            transition: opacity 150ms ease-out;
        }
        "
    );
    widget.style_context().add_provider(&provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    widget.add_css_class("tab-transition");
}

fn create_menu_bar(window: &gtk::ApplicationWindow, buffer: &gtk::TextBuffer, editor_state: Arc<Mutex<EditorState>>, status_label: gtk::Label, text_view: &gtk::TextView) -> (gtk::Box, gtk::Button, gtk::Button, gtk::Button, gtk::Button, gtk::Button, gtk::Box, gtk::Button, gtk::Button, gtk::CheckButton) {
    // Create the main vertical container for menu and tabs
    let main_container = gtk::Box::new(gtk::Orientation::Vertical, 0);
    main_container.set_css_classes(&["main-menu-container"]);
    
    // Create the menu bar (horizontal)
    let menu_bar = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    menu_bar.set_css_classes(&["menu-bar"]);
    
    // Create a more modern File button with icon
    let file_menu_button = gtk::MenuButton::new();
    file_menu_button.set_label("File");
    file_menu_button.set_css_classes(&["menu-button"]);
    file_menu_button.set_has_frame(false);
    file_menu_button.set_focus_on_click(false);
    menu_bar.append(&file_menu_button);
    
    // Create File popup menu
    let menu = gtk::PopoverMenu::from_model(None::<&gtk::gio::MenuModel>);
    let menu_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
    menu_box.set_margin_top(2);
    menu_box.set_margin_bottom(2);
    menu_box.set_margin_start(2);
    menu_box.set_margin_end(2);
    
    // New file button with keyboard shortcut hint
    let new_button = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let new_btn_label = gtk::Label::new(Some("New file"));
    new_btn_label.set_halign(gtk::Align::Start);
    new_btn_label.set_hexpand(true);
    let new_shortcut = gtk::Label::new(Some("Ctrl+T"));
    new_shortcut.set_css_classes(&["dim-label", "shortcut-label"]);
    
    new_button.append(&new_btn_label);
    new_button.append(&new_shortcut);
    
    let new_button_wrapper = gtk::Button::new();
    new_button_wrapper.set_child(Some(&new_button));
    new_button_wrapper.set_has_frame(false);
    new_button_wrapper.set_hexpand(true);
    
    let buffer_ref = buffer.clone();
    let state_ref = editor_state.clone();
    let status_label_ref = status_label.clone();
    new_button_wrapper.connect_clicked(move |_| {
        buffer_ref.set_text("");
        if let Ok(mut state) = state_ref.lock() {
            state.text_buffer.set_text("");
            state.current_file = None;
            state.is_modified = false;
            state.update_tab_name();
            status_label_ref.set_text("Line: 1 Col: 1");
        }
    });
    menu_box.append(&new_button_wrapper);
    
    // Open file button with keyboard shortcut hint
    let open_button = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let open_btn_label = gtk::Label::new(Some("Open file..."));
    open_btn_label.set_halign(gtk::Align::Start);
    open_btn_label.set_hexpand(true);
    let open_shortcut = gtk::Label::new(Some("Ctrl+O"));
    open_shortcut.set_css_classes(&["dim-label", "shortcut-label"]);
    
    open_button.append(&open_btn_label);
    open_button.append(&open_shortcut);
    
    let open_button_wrapper = gtk::Button::new();
    open_button_wrapper.set_child(Some(&open_button));
    open_button_wrapper.set_has_frame(false);
    open_button_wrapper.set_hexpand(true);
    
    let window_ref = window.clone();
    let buffer_ref = buffer.clone();
    let state_ref = editor_state.clone();
    let status_label_ref = status_label.clone();
    open_button_wrapper.connect_clicked(move |_| {
        let dialog = gtk::FileChooserNative::builder()
            .title("Open File")
            .action(gtk::FileChooserAction::Open)
            .accept_label("Open")
            .cancel_label("Cancel")
            .transient_for(&window_ref)
            .modal(true)
            .build();
            
        let filter_text = gtk::FileFilter::new();
        filter_text.add_mime_type("text/plain");
        filter_text.set_name(Some("Text files"));

        let filter_rust = gtk::FileFilter::new();
        filter_rust.add_pattern("*.rs");
        filter_rust.set_name(Some("Rust files"));

        let filter_all = gtk::FileFilter::new();
        filter_all.add_pattern("*");
        filter_all.set_name(Some("All files"));

        dialog.add_filter(&filter_text);
        dialog.add_filter(&filter_rust);
        dialog.add_filter(&filter_all);
        
        let buffer = buffer_ref.clone();
        let state = state_ref.clone();
        let status_label = status_label_ref.clone();
        dialog.connect_response(move |dialog, response| {
            if response == gtk::ResponseType::Accept {
                if let Some(file) = dialog.file() {
                    if let Some(path) = file.path() {
                        match fs::read_to_string(&path) {
                            Ok(content) => {
                                buffer.set_text(&content);
                                if let Ok(mut state) = state.lock() {
                                    if let Err(e) = state.open_file(&path) {
                                        error!("Failed to open file: {}", e);
                                    } else {
                                        state.update_tab_name();
                                        status_label.set_text(&format!("Line: {} Col: {}", 
                                            state.get_cursor_line(), 
                                            state.get_cursor_column()));
                                    }
                                }
                            },
                            Err(e) => {
                                error!("Failed to read file: {}", e);
                            }
                        }
                    }
                }
            }
            dialog.destroy();
        });
        
        dialog.show();
    });
    menu_box.append(&open_button_wrapper);
    
    // Open recent menu item
    let open_recent_button = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let recent_btn_label = gtk::Label::new(Some("Open recent file"));
    recent_btn_label.set_halign(gtk::Align::Start);
    recent_btn_label.set_hexpand(true);
    
    open_recent_button.append(&recent_btn_label);
    
    let open_recent_wrapper = gtk::Button::new();
    open_recent_wrapper.set_child(Some(&open_recent_button));
    open_recent_wrapper.set_has_frame(false);
    open_recent_wrapper.set_hexpand(true);
    
    let buffer_ref = buffer.clone();
    let state_ref = editor_state.clone();
    let status_label_ref = status_label.clone();
    
    open_recent_wrapper.connect_clicked(move |button| {
        // Create a popover for recent files
        let recent_popover = gtk::Popover::new();
        recent_popover.set_parent(button);
        
        let recent_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
        recent_box.set_margin_top(4);
        recent_box.set_margin_bottom(4);
        recent_box.set_margin_start(4);
        recent_box.set_margin_end(4);
        
        let recent_files = {
            if let Ok(state) = state_ref.lock() {
                state.recent_files.get_recent_files().to_vec()
            } else {
                Vec::new()
            }
        };
        
        if recent_files.is_empty() {
            let no_recent_label = gtk::Label::new(Some("No recent files"));
            recent_box.append(&no_recent_label);
        } else {
            for path in recent_files {
                let file_name = path.file_name()
                    .and_then(|f| f.to_str())
                    .unwrap_or("Unknown");
                
                let file_button = gtk::Button::with_label(file_name);
                file_button.set_has_frame(false);
                file_button.set_hexpand(true);
                file_button.set_halign(gtk::Align::Start);
                file_button.set_tooltip_text(Some(&path.to_string_lossy()));
                
                let buffer = buffer_ref.clone();
                let state = state_ref.clone();
                let status_label = status_label_ref.clone();
                let path_clone = path.clone();
                let popover_ref = recent_popover.clone();
                
                file_button.connect_clicked(move |_| {
                    match fs::read_to_string(&path_clone) {
                        Ok(content) => {
                            buffer.set_text(&content);
                            if let Ok(mut state) = state.lock() {
                                if let Err(e) = state.open_file(&path_clone) {
                                    error!("Failed to open file: {}", e);
                                } else {
                                    state.update_tab_name();
                                    status_label.set_text(&format!("Line: {} Col: {}", 
                                        state.get_cursor_line(), 
                                        state.get_cursor_column()));
                                }
                            }
                        },
                        Err(e) => {
                            error!("Failed to read file: {}", e);
                        }
                    }
                    popover_ref.popdown();
                });
                
                recent_box.append(&file_button);
            }
        }
        
        recent_popover.set_child(Some(&recent_box));
        recent_popover.popup();
    });
    menu_box.append(&open_recent_wrapper);
    
    // Add separator
    let separator1 = gtk::Separator::new(gtk::Orientation::Horizontal);
    separator1.set_margin_top(2);
    separator1.set_margin_bottom(2);
    menu_box.append(&separator1);
    
    // Save file button with keyboard shortcut hint
    let save_button = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let save_btn_label = gtk::Label::new(Some("Save"));
    save_btn_label.set_halign(gtk::Align::Start);
    save_btn_label.set_hexpand(true);
    let save_shortcut = gtk::Label::new(Some("Ctrl+S"));
    save_shortcut.set_css_classes(&["dim-label", "shortcut-label"]);
    
    save_button.append(&save_btn_label);
    save_button.append(&save_shortcut);
    
    let save_button_wrapper = gtk::Button::new();
    save_button_wrapper.set_child(Some(&save_button));
    save_button_wrapper.set_has_frame(false);
    save_button_wrapper.set_hexpand(true);
    
    let window_ref = window.clone();
    let buffer_ref = buffer.clone();
    let state_ref = editor_state.clone();
    save_button_wrapper.connect_clicked(move |_| {
        let should_show_dialog = {
            if let Ok(state) = state_ref.lock() {
                state.current_file.is_none()
            } else {
                true
            }
        };
        
        if should_show_dialog {
            let dialog = gtk::FileChooserNative::builder()
                .title("Save File")
                .action(gtk::FileChooserAction::Save)
                .accept_label("Save")
                .cancel_label("Cancel")
                .transient_for(&window_ref)
                .modal(true)
                .build();
                
            let filter_text = gtk::FileFilter::new();
            filter_text.add_mime_type("text/plain");
            filter_text.set_name(Some("Text files"));

            let filter_rust = gtk::FileFilter::new();
            filter_rust.add_pattern("*.rs");
            filter_rust.set_name(Some("Rust files"));

            let filter_all = gtk::FileFilter::new();
            filter_all.add_pattern("*");
            filter_all.set_name(Some("All files"));

            dialog.add_filter(&filter_text);
            dialog.add_filter(&filter_rust);
            dialog.add_filter(&filter_all);
            
            let buffer = buffer_ref.clone();
            let state = state_ref.clone();
            dialog.connect_response(move |dialog, response| {
                if response == gtk::ResponseType::Accept {
                    if let Some(file) = dialog.file() {
                        if let Some(path) = file.path() {
                            let text = buffer.text(&buffer.start_iter(), &buffer.end_iter(), false);
                            match fs::write(&path, text.as_str()) {
                                Ok(_) => {
                                    if let Ok(mut state) = state.lock() {
                                        state.current_file = Some(path.clone());
                                        state.is_modified = false;
                                        state.recent_files.add_file(path);
                                        state.update_tab_name();
                                    }
                                },
                                Err(e) => {
                                    error!("Failed to save file: {}", e);
                                }
                            }
                        }
                    }
                }
                dialog.destroy();
            });
            
            dialog.show();
        } else {
            // Save to existing file
            if let Ok(mut state) = state_ref.lock() {
                if let Some(path) = &state.current_file {
                    let text = buffer_ref.text(&buffer_ref.start_iter(), &buffer_ref.end_iter(), false);
                    match fs::write(path, text.as_str()) {
                        Ok(_) => {
                            state.is_modified = false;
                        },
                        Err(e) => {
                            error!("Failed to save file: {}", e);
                        }
                    }
                }
            }
        }
    });
    menu_box.append(&save_button_wrapper);
    
    // Save As button with keyboard shortcut hint
    let save_as_button = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let save_as_btn_label = gtk::Label::new(Some("Save as..."));
    save_as_btn_label.set_halign(gtk::Align::Start);
    save_as_btn_label.set_hexpand(true);
    let save_as_shortcut = gtk::Label::new(Some("Ctrl+Shift+S"));
    save_as_shortcut.set_css_classes(&["dim-label", "shortcut-label"]);
    
    save_as_button.append(&save_as_btn_label);
    save_as_button.append(&save_as_shortcut);
    
    let save_as_button_wrapper = gtk::Button::new();
    save_as_button_wrapper.set_child(Some(&save_as_button));
    save_as_button_wrapper.set_has_frame(false);
    save_as_button_wrapper.set_hexpand(true);
    
    let window_ref = window.clone();
    let buffer_ref = buffer.clone();
    let state_ref = editor_state.clone();
    save_as_button_wrapper.connect_clicked(move |_| {
        let dialog = gtk::FileChooserNative::builder()
            .title("Save File As")
            .action(gtk::FileChooserAction::Save)
            .accept_label("Save")
            .cancel_label("Cancel")
            .transient_for(&window_ref)
            .modal(true)
            .build();
            
        let filter_text = gtk::FileFilter::new();
        filter_text.add_mime_type("text/plain");
        filter_text.set_name(Some("Text files"));

        let filter_rust = gtk::FileFilter::new();
        filter_rust.add_pattern("*.rs");
        filter_rust.set_name(Some("Rust files"));

        let filter_all = gtk::FileFilter::new();
        filter_all.add_pattern("*");
        filter_all.set_name(Some("All files"));

        dialog.add_filter(&filter_text);
        dialog.add_filter(&filter_rust);
        dialog.add_filter(&filter_all);
        
        // Set current filename if available
        if let Ok(state) = state_ref.lock() {
            if let Some(path) = &state.current_file {
                if let Some(name) = path.file_name() {
                    dialog.set_current_name(&name.to_string_lossy());
                }
            }
        }
        
        let buffer = buffer_ref.clone();
        let state = state_ref.clone();
        dialog.connect_response(move |dialog, response| {
            if response == gtk::ResponseType::Accept {
                if let Some(file) = dialog.file() {
                    if let Some(path) = file.path() {
                        let text = buffer.text(&buffer.start_iter(), &buffer.end_iter(), false);
                        match fs::write(&path, text.as_str()) {
                            Ok(_) => {
                                if let Ok(mut state) = state.lock() {
                                    state.current_file = Some(path.clone());
                                    state.is_modified = false;
                                    state.recent_files.add_file(path);
                                    state.update_tab_name();
                                }
                            },
                            Err(e) => {
                                error!("Failed to save file: {}", e);
                            }
                        }
                    }
                }
            }
            dialog.destroy();
        });
        
        dialog.show();
    });
    menu_box.append(&save_as_button_wrapper);
    
    // Add separator
    let separator2 = gtk::Separator::new(gtk::Orientation::Horizontal);
    separator2.set_margin_top(2);
    separator2.set_margin_bottom(2);
    menu_box.append(&separator2);
    
    // Close file button with keyboard shortcut hint
    let close_button = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let close_btn_label = gtk::Label::new(Some("Close file"));
    close_btn_label.set_halign(gtk::Align::Start);
    close_btn_label.set_hexpand(true);
    let close_shortcut = gtk::Label::new(Some("Ctrl+W"));
    close_shortcut.set_css_classes(&["dim-label", "shortcut-label"]);
    
    close_button.append(&close_btn_label);
    close_button.append(&close_shortcut);
    
    let close_button_wrapper = gtk::Button::new();
    close_button_wrapper.set_child(Some(&close_button));
    close_button_wrapper.set_has_frame(false);
    close_button_wrapper.set_hexpand(true);
    
    let buffer_ref = buffer.clone();
    let state_ref = editor_state.clone();
    close_button_wrapper.connect_clicked(move |_| {
        buffer_ref.set_text("");
        if let Ok(mut state) = state_ref.lock() {
            state.text_buffer.set_text("");
            state.current_file = None;
            state.is_modified = false;
            state.update_tab_name();
        }
    });
    menu_box.append(&close_button_wrapper);
    
    // Add separator before quit
    let separator3 = gtk::Separator::new(gtk::Orientation::Horizontal);
    separator3.set_margin_top(2);
    separator3.set_margin_bottom(2);
    menu_box.append(&separator3);
    
    // Quit button with keyboard shortcut hint
    let quit_button = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let quit_btn_label = gtk::Label::new(Some("Quit"));
    quit_btn_label.set_halign(gtk::Align::Start);
    quit_btn_label.set_hexpand(true);
    let quit_shortcut = gtk::Label::new(Some("Ctrl+Q"));
    quit_shortcut.set_css_classes(&["dim-label", "shortcut-label"]);
    
    quit_button.append(&quit_btn_label);
    quit_button.append(&quit_shortcut);
    
    let quit_button_wrapper = gtk::Button::new();
    quit_button_wrapper.set_child(Some(&quit_button));
    quit_button_wrapper.set_has_frame(false);
    quit_button_wrapper.set_hexpand(true);
    
    let app_window = window.clone();
    quit_button_wrapper.connect_clicked(move |_| {
        app_window.close();
    });
    menu_box.append(&quit_button_wrapper);
    
    menu.set_child(Some(&menu_box));
    file_menu_button.set_popover(Some(&menu));
    
    // Add Edit menu button next to File
    let edit_menu_button = gtk::MenuButton::new();
    edit_menu_button.set_label("Edit");
    edit_menu_button.set_css_classes(&["menu-button"]);
    edit_menu_button.set_has_frame(false);
    edit_menu_button.set_focus_on_click(false);
    menu_bar.append(&edit_menu_button);

    // Create Edit popup menu
    let edit_menu = gtk::PopoverMenu::from_model(None::<&gtk::gio::MenuModel>);
    let edit_menu_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
    edit_menu_box.set_margin_top(2);
    edit_menu_box.set_margin_bottom(2);
    edit_menu_box.set_margin_start(2);
    edit_menu_box.set_margin_end(2);

    // Undo button with keyboard shortcut hint
    let undo_button = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let undo_btn_label = gtk::Label::new(Some("Undo"));
    undo_btn_label.set_halign(gtk::Align::Start);
    undo_btn_label.set_hexpand(true);
    let undo_shortcut = gtk::Label::new(Some("Ctrl+Z"));
    undo_shortcut.set_css_classes(&["dim-label", "shortcut-label"]);
    
    undo_button.append(&undo_btn_label);
    undo_button.append(&undo_shortcut);
    
    let undo_button_wrapper = gtk::Button::new();
    undo_button_wrapper.set_child(Some(&undo_button));
    undo_button_wrapper.set_has_frame(false);
    undo_button_wrapper.set_hexpand(true);
    
    let buffer_ref = buffer.clone();
    let state_ref = editor_state.clone();
    undo_button_wrapper.connect_clicked(move |_| {
        if let Ok(mut state) = state_ref.lock() {
            if let Some(previous_text) = state.undo() {
                buffer_ref.set_text(&previous_text);
                state.text_buffer.set_text(&previous_text);
            }
        }
    });
    edit_menu_box.append(&undo_button_wrapper);

    // Redo button with keyboard shortcut hint
    let redo_button = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let redo_btn_label = gtk::Label::new(Some("Redo"));
    redo_btn_label.set_halign(gtk::Align::Start);
    redo_btn_label.set_hexpand(true);
    let redo_shortcut = gtk::Label::new(Some("Ctrl+Y"));
    redo_shortcut.set_css_classes(&["dim-label", "shortcut-label"]);
    
    redo_button.append(&redo_btn_label);
    redo_button.append(&redo_shortcut);
    
    let redo_button_wrapper = gtk::Button::new();
    redo_button_wrapper.set_child(Some(&redo_button));
    redo_button_wrapper.set_has_frame(false);
    redo_button_wrapper.set_hexpand(true);
    
    let buffer_ref = buffer.clone();
    let state_ref = editor_state.clone();
    redo_button_wrapper.connect_clicked(move |_| {
        if let Ok(mut state) = state_ref.lock() {
            if let Some(next_text) = state.redo() {
                buffer_ref.set_text(&next_text);
                state.text_buffer.set_text(&next_text);
            }
        }
    });
    edit_menu_box.append(&redo_button_wrapper);

    // Add separator
    let separator_edit = gtk::Separator::new(gtk::Orientation::Horizontal);
    separator_edit.set_margin_top(2);
    separator_edit.set_margin_bottom(2);
    edit_menu_box.append(&separator_edit);

    // Find button
    let find_button = gtk::Button::with_label("Find...");
    find_button.set_has_frame(false);
    find_button.set_hexpand(true);
    find_button.set_halign(gtk::Align::Start);
    edit_menu_box.append(&find_button);

    // Replace button
    let replace_button = gtk::Button::with_label("Replace...");
    replace_button.set_has_frame(false);
    replace_button.set_hexpand(true);
    replace_button.set_halign(gtk::Align::Start);
    edit_menu_box.append(&replace_button);

    edit_menu.set_child(Some(&edit_menu_box));
    edit_menu_button.set_popover(Some(&edit_menu));
    
    // Add View menu button after Edit
    let view_menu_button = gtk::MenuButton::new();
    view_menu_button.set_label("View");
    view_menu_button.set_css_classes(&["menu-button"]);
    view_menu_button.set_has_frame(false);
    view_menu_button.set_focus_on_click(false);
    menu_bar.append(&view_menu_button);

    // Create View popup menu
    let view_menu = gtk::PopoverMenu::from_model(None::<&gtk::gio::MenuModel>);
    let view_menu_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
    view_menu_box.set_margin_top(2);
    view_menu_box.set_margin_bottom(2);
    view_menu_box.set_margin_start(2);
    view_menu_box.set_margin_end(2);

    // Word Wrap toggle
    let word_wrap_button = gtk::CheckButton::with_label("Word Wrap");
    word_wrap_button.set_active(false);
    view_menu_box.append(&word_wrap_button);

    // Show Line Numbers toggle
    let show_line_numbers_button = gtk::CheckButton::with_label("Show Line Numbers");
    show_line_numbers_button.set_active(true);
    view_menu_box.append(&show_line_numbers_button);

    // Add separator
    let separator_view1 = gtk::Separator::new(gtk::Orientation::Horizontal);
    separator_view1.set_margin_top(2);
    separator_view1.set_margin_bottom(2);
    view_menu_box.append(&separator_view1);

    // Zoom In button with keyboard shortcut hint
    let zoom_in_button = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let zoom_in_label = gtk::Label::new(Some("Zoom In"));
    zoom_in_label.set_halign(gtk::Align::Start);
    zoom_in_label.set_hexpand(true);
    let zoom_in_shortcut = gtk::Label::new(Some("Ctrl++"));
    zoom_in_shortcut.set_css_classes(&["dim-label", "shortcut-label"]);

    zoom_in_button.append(&zoom_in_label);
    zoom_in_button.append(&zoom_in_shortcut);

    let zoom_in_wrapper = gtk::Button::new();
    zoom_in_wrapper.set_child(Some(&zoom_in_button));
    zoom_in_wrapper.set_has_frame(false);
    zoom_in_wrapper.set_hexpand(true);

    let state_ref = editor_state.clone();
    let text_view_ref = text_view.clone();
    zoom_in_wrapper.connect_clicked(move |_| {
        if let Ok(mut state) = state_ref.lock() {
            state.zoom_in();
            apply_zoom(&text_view_ref, state.zoom_level);
        }
    });
    view_menu_box.append(&zoom_in_wrapper);

    // Zoom Out button with keyboard shortcut hint
    let zoom_out_button = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let zoom_out_label = gtk::Label::new(Some("Zoom Out"));
    zoom_out_label.set_halign(gtk::Align::Start);
    zoom_out_label.set_hexpand(true);
    let zoom_out_shortcut = gtk::Label::new(Some("Ctrl+-"));
    zoom_out_shortcut.set_css_classes(&["dim-label", "shortcut-label"]);

    zoom_out_button.append(&zoom_out_label);
    zoom_out_button.append(&zoom_out_shortcut);

    let zoom_out_wrapper = gtk::Button::new();
    zoom_out_wrapper.set_child(Some(&zoom_out_button));
    zoom_out_wrapper.set_has_frame(false);
    zoom_out_wrapper.set_hexpand(true);

    let state_ref = editor_state.clone();
    let text_view_ref = text_view.clone();
    zoom_out_wrapper.connect_clicked(move |_| {
        if let Ok(mut state) = state_ref.lock() {
            state.zoom_out();
            apply_zoom(&text_view_ref, state.zoom_level);
        }
    });
    view_menu_box.append(&zoom_out_wrapper);

    // Reset Zoom button with keyboard shortcut hint
    let reset_zoom_button = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let reset_zoom_label = gtk::Label::new(Some("Reset Zoom"));
    reset_zoom_label.set_halign(gtk::Align::Start);
    reset_zoom_label.set_hexpand(true);
    let reset_zoom_shortcut = gtk::Label::new(Some("Ctrl+0"));
    reset_zoom_shortcut.set_css_classes(&["dim-label", "shortcut-label"]);

    reset_zoom_button.append(&reset_zoom_label);
    reset_zoom_button.append(&reset_zoom_shortcut);

    let reset_zoom_wrapper = gtk::Button::new();
    reset_zoom_wrapper.set_child(Some(&reset_zoom_button));
    reset_zoom_wrapper.set_has_frame(false);
    reset_zoom_wrapper.set_hexpand(true);

    let state_ref = editor_state.clone();
    let text_view_ref = text_view.clone();
    reset_zoom_wrapper.connect_clicked(move |_| {
        if let Ok(mut state) = state_ref.lock() {
            state.reset_zoom();
            apply_zoom(&text_view_ref, state.zoom_level);
        }
    });
    view_menu_box.append(&reset_zoom_wrapper);

    view_menu.set_child(Some(&view_menu_box));
    view_menu_button.set_popover(Some(&view_menu));

    // Connect word wrap toggle
    let text_view_ref = text_view.clone();
    word_wrap_button.connect_toggled(move |button| {
        if button.is_active() {
            text_view_ref.set_wrap_mode(gtk::WrapMode::Word);
        } else {
            text_view_ref.set_wrap_mode(gtk::WrapMode::None);
        }
    });

    // Add Help menu button
    let help_menu_button = gtk::MenuButton::new();
    help_menu_button.set_label("Help");
    help_menu_button.set_css_classes(&["menu-button"]);
    help_menu_button.set_has_frame(false);
    help_menu_button.set_focus_on_click(false);
    menu_bar.append(&help_menu_button);

    // Create Help popup menu
    let help_menu = gtk::PopoverMenu::from_model(None::<&gtk::gio::MenuModel>);
    let help_menu_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
    help_menu_box.set_margin_top(2);
    help_menu_box.set_margin_bottom(2);
    help_menu_box.set_margin_start(2);
    help_menu_box.set_margin_end(2);

    // Keyboard Shortcuts button
    let shortcuts_button = gtk::Button::with_label("Keyboard Shortcuts");
    shortcuts_button.set_has_frame(false);
    shortcuts_button.set_hexpand(true);
    shortcuts_button.set_halign(gtk::Align::Start);

    let window_ref = window.clone();
    shortcuts_button.connect_clicked(move |_| {
        // Create a dialog with keyboard shortcuts
        let dialog = gtk::Dialog::with_buttons(
            Some("Keyboard Shortcuts"),
            Some(&window_ref),
            gtk::DialogFlags::MODAL | gtk::DialogFlags::DESTROY_WITH_PARENT,
            &[("Close", gtk::ResponseType::Close)],
        );
        dialog.set_default_width(400);
        dialog.set_default_height(500);
        
        let content_area = dialog.content_area();
        content_area.set_margin_top(10);
        content_area.set_margin_bottom(10);
        content_area.set_margin_start(10);
        content_area.set_margin_end(10);
        
        let shortcuts_box = gtk::Box::new(gtk::Orientation::Vertical, 10);
        
        // File Operations shortcuts
        let file_label = gtk::Label::new(Some("File Operations"));
        file_label.set_halign(gtk::Align::Start);
        file_label.set_css_classes(&["heading"]);
        shortcuts_box.append(&file_label);
        
        let shortcuts = [
            ("New File", "Ctrl+T"),
            ("Open File", "Ctrl+O"),
            ("Save", "Ctrl+S"),
            ("Save As", "Ctrl+Shift+S"),
            ("Close File", "Ctrl+W"),
            ("Quit", "Ctrl+Q"),
        ];
        
        let file_grid = gtk::Grid::new();
        file_grid.set_column_spacing(20);
        file_grid.set_row_spacing(5);
        file_grid.set_margin_start(10);
        
        for (i, (action, shortcut)) in shortcuts.iter().enumerate() {
            let action_label = gtk::Label::new(Some(action));
            action_label.set_halign(gtk::Align::Start);
            
            let shortcut_label = gtk::Label::new(Some(shortcut));
            shortcut_label.set_halign(gtk::Align::Start);
            
            file_grid.attach(&action_label, 0, i as i32, 1, 1);
            file_grid.attach(&shortcut_label, 1, i as i32, 1, 1);
        }
        
        shortcuts_box.append(&file_grid);
        
        // Edit Operations shortcuts
        let edit_label = gtk::Label::new(Some("Edit Operations"));
        edit_label.set_halign(gtk::Align::Start);
        edit_label.set_css_classes(&["heading"]);
        edit_label.set_margin_top(10);
        shortcuts_box.append(&edit_label);
        
        let edit_shortcuts = [
            ("Undo", "Ctrl+Z"),
            ("Redo", "Ctrl+Y"),
            ("Find", "Ctrl+F"),
            ("Replace", "Ctrl+H"),
        ];
        
        let edit_grid = gtk::Grid::new();
        edit_grid.set_column_spacing(20);
        edit_grid.set_row_spacing(5);
        edit_grid.set_margin_start(10);
        
        for (i, (action, shortcut)) in edit_shortcuts.iter().enumerate() {
            let action_label = gtk::Label::new(Some(action));
            action_label.set_halign(gtk::Align::Start);
            
            let shortcut_label = gtk::Label::new(Some(shortcut));
            shortcut_label.set_halign(gtk::Align::Start);
            
            edit_grid.attach(&action_label, 0, i as i32, 1, 1);
            edit_grid.attach(&shortcut_label, 1, i as i32, 1, 1);
        }
        
        shortcuts_box.append(&edit_grid);
        
        // View Operations shortcuts
        let view_label = gtk::Label::new(Some("View Operations"));
        view_label.set_halign(gtk::Align::Start);
        view_label.set_css_classes(&["heading"]);
        view_label.set_margin_top(10);
        shortcuts_box.append(&view_label);
        
        let view_shortcuts = [
            ("Zoom In", "Ctrl++"),
            ("Zoom Out", "Ctrl+-"),
            ("Reset Zoom", "Ctrl+0"),
        ];
        
        let view_grid = gtk::Grid::new();
        view_grid.set_column_spacing(20);
        view_grid.set_row_spacing(5);
        view_grid.set_margin_start(10);
        
        for (i, (action, shortcut)) in view_shortcuts.iter().enumerate() {
            let action_label = gtk::Label::new(Some(action));
            action_label.set_halign(gtk::Align::Start);
            
            let shortcut_label = gtk::Label::new(Some(shortcut));
            shortcut_label.set_halign(gtk::Align::Start);
            
            view_grid.attach(&action_label, 0, i as i32, 1, 1);
            view_grid.attach(&shortcut_label, 1, i as i32, 1, 1);
        }
        
        shortcuts_box.append(&view_grid);
        
        let scrolled_window = gtk::ScrolledWindow::new();
        scrolled_window.set_child(Some(&shortcuts_box));
        scrolled_window.set_vexpand(true);
        
        content_area.append(&scrolled_window);
        
        dialog.connect_response(|dialog, _| {
            dialog.destroy();
        });
        
        dialog.show();
    });
    help_menu_box.append(&shortcuts_button);

    // About button
    let about_button = gtk::Button::with_label("About RustEdit");
    about_button.set_has_frame(false);
    about_button.set_hexpand(true);
    about_button.set_halign(gtk::Align::Start);

    let window_ref = window.clone();
    about_button.connect_clicked(move |_| {
        let dialog = gtk::AboutDialog::new();
        dialog.set_modal(true);
        dialog.set_transient_for(Some(&window_ref));
        dialog.set_program_name(Some("RustEdit"));
        dialog.set_version(Some("0.1.0"));
        dialog.set_comments(Some("A lightweight text editor inspired by COSMIC Edit"));
        dialog.set_copyright(Some("© 2023 RustEdit Developers"));
        dialog.set_license_type(gtk::License::Gpl30);
        
        dialog.show();
    });
    help_menu_box.append(&about_button);

    help_menu.set_child(Some(&help_menu_box));
    help_menu_button.set_popover(Some(&help_menu));
    
    // Create a separator between menu bars and tabs
    let separator = gtk::Separator::new(gtk::Orientation::Horizontal);
    separator.set_css_classes(&["menu-separator"]);
    
    // Add the menu bar to the main container
    main_container.append(&menu_bar);
    main_container.append(&separator);
    
    // Create a new separate row for tabs (horizontal box)
    let tabs_row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    tabs_row.set_css_classes(&["tabs-row"]);
    
    // Add modern tab bar container
    let tabs_container = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    tabs_container.set_hexpand(true);
    tabs_container.set_css_classes(&["tab-bar"]);
    
    // Create tabs box and store tab buttons in a Vec for tracking
    let tabs_box = gtk::Box::new(gtk::Orientation::Horizontal, 2);
    tabs_box.set_hexpand(true);
    tabs_box.set_css_classes(&["tabs-box"]);
    
    // Create tab button with modern styling
    let tab_button = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    tab_button.set_css_classes(&["tab-button"]);
    
    // Get the tab name
    let tab_name = {
        if let Ok(state) = editor_state.lock() {
            state.tab_name.clone()
        } else {
            "Untitled".to_string()
        }
    };
    
    // Create a label for the tab
    let tab_label = gtk::Label::new(Some(&tab_name));
    tab_label.set_css_classes(&["tab-label"]);
    tab_label.set_ellipsize(pango::EllipsizeMode::End);
    tab_label.set_width_chars(15);
    tab_label.set_max_width_chars(15);
    
    // Create a close button for the tab
    let close_icon = gtk::Button::new();
    close_icon.set_css_classes(&["tab-close-button"]);
    close_icon.set_icon_name("window-close-symbolic");
    close_icon.set_tooltip_text(Some("Close tab"));
    
    // Add elements to tab button
    tab_button.append(&tab_label);
    tab_button.append(&close_icon);
    
    // Wrap tab button in a clickable button
    let tab_button_wrapper = gtk::Button::new();
    tab_button_wrapper.set_css_classes(&["tab-button-wrapper", "active"]);
    tab_button_wrapper.set_has_frame(false);
    tab_button_wrapper.set_child(Some(&tab_button));
    
    // Add the tab to tabs box
    tabs_box.append(&tab_button_wrapper);
    
    // Create a "+" button to add new tabs with modern styling
    let new_tab_button = gtk::Button::new();
    new_tab_button.set_icon_name("list-add-symbolic");
    new_tab_button.set_tooltip_text(Some("New Tab"));
    new_tab_button.set_css_classes(&["new-tab-button"]);
    
    // Add the new tab button after the first tab
    tabs_box.append(&new_tab_button);
    
    // Connect the initial tab to activate it when clicked
    let text_view_ref = text_view.clone();
    let buffer_clone = buffer.clone();
    let tab_button_wrapper_clone = tab_button_wrapper.clone();
    
    tab_button_wrapper.connect_clicked(move |clicked_button| {
        // Set this tab as active
        clicked_button.set_css_classes(&["tab-button-wrapper", "active"]);
        
        // Switch to this tab's buffer
        text_view_ref.set_buffer(Some(&buffer_clone));
    });
    
    // Make the close button for the first tab work
    let buffer_clone = buffer.clone();
    let editor_state_ref = editor_state.clone();
    
    // Create a gesture controller for the first tab's close button
    let first_click_controller = gtk::GestureClick::new();
    first_click_controller.set_button(1); // Left mouse button
    first_click_controller.set_propagation_phase(gtk::PropagationPhase::Capture);
    close_icon.add_controller(first_click_controller.clone());
    
    let buffer_clone = buffer.clone();
    let editor_state_ref = editor_state.clone();
    let text_view_ref = text_view.clone();
    
    first_click_controller.connect_pressed(move |gesture, _, _, _| {
        debug!("First tab X button clicked");
        gesture.set_state(gtk::EventSequenceState::Claimed);
        
        // Ask if they want to close the tab if content is modified
        if let Ok(state) = editor_state_ref.lock() {
            if state.is_modified {
                debug!("First tab has modified content, just clearing instead of closing");
                buffer_clone.set_text("");
                return;
            }
        }
        
        debug!("Clearing content of first tab (not removing it as it's the primary tab)");
        // Just clear the content of this tab as it's the main tab
        // We don't actually remove this tab as it's the primary one
        buffer_clone.set_text("");
        
        // Reset any file association
        if let Ok(mut state) = editor_state_ref.lock() {
            state.current_file = None;
            state.is_modified = false;
            state.update_tab_name();
        }
        
        // Ensure we're showing the first tab's buffer
        text_view_ref.set_buffer(Some(&buffer_clone));
    });
    
    // Set up a timer to update the tab label when state changes (like when a file is opened)
    let editor_state_ref = editor_state.clone();
    let tab_label_ref = tab_label.clone();
    
    let timeout_id = glib::timeout_add_local(Duration::from_millis(500), move || {
        if let Ok(state) = editor_state_ref.lock() {
            tab_label_ref.set_text(&state.tab_name);
        }
        // Continue the timer
        glib::ControlFlow::Continue
    });
    
    // Store the timeout ID
    if let Ok(mut state) = editor_state.lock() {
        state.timeout_id = Some(timeout_id);
    }
    
    // Add right-click context menu for the first tab
    let gesture = gtk::GestureClick::new();
    gesture.set_button(3); // Right mouse button
    
    let tab_button_wrapper_ref = tab_button_wrapper.clone();
    // Create a fresh buffer clone for this closure
    let buffer_for_context = buffer.clone();
    
    gesture.connect_pressed(move |_, _, _, _| {
        let popover = gtk::Popover::new();
        popover.set_parent(&tab_button_wrapper_ref);
        
        let box_container = gtk::Box::new(gtk::Orientation::Vertical, 5);
        box_container.set_margin_top(5);
        box_container.set_margin_bottom(5);
        box_container.set_margin_start(5);
        box_container.set_margin_end(5);
        
        // Clear tab content option
        let clear_item = gtk::Button::new();
        clear_item.set_label("Clear Content");
        clear_item.set_css_classes(&["menu-item"]);
        clear_item.set_has_frame(false);
        
        // Use clone specific to this inner closure
        let buffer_for_clear = buffer_for_context.clone();
        let popover_ref = popover.clone();
        
        let clear_item_clone = clear_item.clone();
        clear_item.connect_clicked(move |_| {
            buffer_for_clear.set_text("");
            popover_ref.popdown();
        });
        
        box_container.append(&clear_item_clone);
        
        popover.set_child(Some(&box_container));
        popover.popup();
    });
    
    tab_button_wrapper.add_controller(gesture);
    
    // Connect the + button to create a new tab
    let tabs_box_ref = tabs_box.clone();
    let new_tab_button_ref = new_tab_button.clone();
    let editor_state_ref = editor_state.clone();
    let text_view_ref = text_view.clone();
    let tab_button_wrapper_ref = tab_button_wrapper.clone();
    // Create a fresh owned buffer for the new tab handler
    let buffer_for_new_tab = buffer.clone();
    
    new_tab_button.connect_clicked(move |_| {
        // Create a new buffer with syntax highlighting
        let tag_table = create_tag_table();
        let new_buffer = TextBuffer::new(Some(&tag_table));
        
        // Generate tab ID
        let tab_id = {
            if let Ok(mut state) = editor_state_ref.lock() {
                state.active_tab_id += 1;
                state.active_tab_id
            } else {
                0
            }
        };
        
        // Create new tab with initial opacity of 0
        let new_tab_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        new_tab_box.set_css_classes(&["tab-button"]);
        new_tab_box.set_opacity(0.0);
        create_tab_transition(&new_tab_box);
        
        let new_tab_label = gtk::Label::new(Some(&format!("Untitled {}", tab_id)));
        new_tab_label.set_css_classes(&["tab-label"]);
        new_tab_label.set_ellipsize(pango::EllipsizeMode::End);
        new_tab_label.set_width_chars(15);
        new_tab_label.set_max_width_chars(15);
        
        let new_close_icon = gtk::Button::new();
        new_close_icon.set_css_classes(&["tab-close-button"]);
        new_close_icon.set_icon_name("window-close-symbolic");
        new_close_icon.set_tooltip_text(Some("Close tab"));
        
        new_tab_box.append(&new_tab_label);
        new_tab_box.append(&new_close_icon);
        
        let new_tab_wrapper = gtk::Button::new();
        new_tab_wrapper.set_css_classes(&["tab-button-wrapper"]);
        new_tab_wrapper.set_has_frame(false);
        new_tab_wrapper.set_child(Some(&new_tab_box));
        
        // Add the tab to the box first
        tabs_box_ref.remove(&new_tab_button_ref);
        tabs_box_ref.append(&new_tab_wrapper);
        tabs_box_ref.append(&new_tab_button_ref);
        
        // Use a timeout to trigger the fade-in
        glib::timeout_add_local(Duration::from_millis(50), move || {
            new_tab_box.set_opacity(1.0);
            glib::ControlFlow::Break
        });
        
        // Connect close button - we need a fresh buffer for each tab
        let tabs_box_ref_clone = tabs_box_ref.clone();
        let new_tab_wrapper_clone = new_tab_wrapper.clone();
        let text_view_ref_clone = text_view_ref.clone();
        // Create a fresh buffer clone specific to this closure
        let buffer_for_close = buffer_for_new_tab.clone();
        let tab_button_wrapper_ref_clone = tab_button_wrapper_ref.clone();
        
        // CRITICAL: Create separate click controller for close button to ensure clicks are captured
        let click_controller = gtk::GestureClick::new();
        click_controller.set_button(1); // Left mouse button
        click_controller.set_propagation_phase(gtk::PropagationPhase::Capture);
        new_close_icon.add_controller(click_controller.clone());
        
        let tabs_box_ref_clone = tabs_box_ref.clone();
        let new_tab_wrapper_clone = new_tab_wrapper.clone();
        let text_view_ref_clone = text_view_ref.clone();
        let buffer_for_close = buffer_for_new_tab.clone();
        let tab_button_wrapper_ref_clone = tab_button_wrapper_ref.clone();
        
        click_controller.connect_pressed(move |gesture, _, _, _| {
            debug!("Tab X button clicked");
            gesture.set_state(gtk::EventSequenceState::Claimed);
            
            // Check if this is the active tab
            let is_active = new_tab_wrapper_clone.css_classes().iter().any(|class| class == "active");
            debug!("Is active tab: {}", is_active);
            
            // Create fade-out transition
            create_tab_transition(&new_tab_wrapper_clone);
            
            // Start the fade-out
            new_tab_wrapper_clone.set_opacity(0.0);
            
            // Clone all the necessary variables for the inner closure
            let tabs_box_ref_inner = tabs_box_ref_clone.clone();
            let new_tab_wrapper_inner = new_tab_wrapper_clone.clone();
            let text_view_ref_inner = text_view_ref_clone.clone();
            let buffer_for_close_inner = buffer_for_close.clone();
            let tab_button_wrapper_ref_inner = tab_button_wrapper_ref_clone.clone();
            let is_active_inner = is_active;
            
            glib::timeout_add_local(Duration::from_millis(150), move || {
                // Remove the tab after animation completes
                tabs_box_ref_inner.remove(&new_tab_wrapper_inner);
                
                // Check if the tab was actually removed
                if new_tab_wrapper_inner.parent().is_some() {
                    warn!("Tab wasn't removed properly, it still has a parent");
                } else {
                    debug!("Tab was successfully removed");
                }
                
                // If this was the active tab, switch back to the first tab
                if is_active_inner {
                    debug!("Switching back to first tab since active tab was closed");
                    text_view_ref_inner.set_buffer(Some(&buffer_for_close_inner));
                    tab_button_wrapper_ref_inner.set_css_classes(&["tab-button-wrapper", "active"]);
                }
                
                glib::ControlFlow::Break
            });
        });
        
        // Connect tab button to switch to this tab
        let new_buffer_clone = new_buffer.clone();
        let text_view_ref_clone = text_view_ref.clone();
        let tab_button_wrapper_clone = tab_button_wrapper_ref.clone();
        
        new_tab_wrapper.connect_clicked(move |clicked_button| {
            // Set all tabs to inactive (simplified approach)
            if let Some(parent) = clicked_button.parent() {
                if let Some(box_parent) = parent.downcast_ref::<gtk::Box>() {
                    // Find all buttons in the tabs box and set them to inactive
                    let n_children = box_parent.first_child()
                        .map(|_| {
                            let mut count = 0;
                            let mut child = box_parent.first_child();
                            while let Some(widget) = child {
                                count += 1;
                                child = widget.next_sibling();
                            }
                            count
                        })
                        .unwrap_or(0);

                    let mut child = box_parent.first_child();
                    for _ in 0..n_children {
                        if let Some(widget) = child.clone() {
                            if let Some(button) = widget.downcast_ref::<gtk::Button>() {
                                // Don't compare pointers, just set all to inactive
                                button.set_css_classes(&["tab-button-wrapper"]);
                            }
                            child = widget.next_sibling();
                        }
                    }
                }
            }
            
            // Set this tab as active
            clicked_button.set_css_classes(&["tab-button-wrapper", "active"]);
            // Set old tab to inactive
            tab_button_wrapper_clone.set_css_classes(&["tab-button-wrapper"]);
            
            // Set this tab as active
            clicked_button.set_css_classes(&["tab-button-wrapper", "active"]);
            
            // Switch to this tab's buffer
            text_view_ref_clone.set_buffer(Some(&new_buffer_clone));
        });
        
        // Add right-click context menu for the new tab
        let right_click = gtk::GestureClick::new();
        right_click.set_button(3); // Right mouse button
        
        let new_tab_wrapper_ref = new_tab_wrapper.clone();
        let tabs_box_ref_clone = tabs_box_ref.clone();
        let text_view_ref_clone = text_view_ref.clone();
        // Create separate buffer clones to avoid lifetime issues
        let buffer_for_menu = buffer_for_new_tab.clone();
        let tab_button_wrapper_ref_clone = tab_button_wrapper_ref.clone();
        let new_buffer_for_menu = new_buffer.clone();
        
        right_click.connect_pressed(move |_, _, _, _| {
            let popover = gtk::Popover::new();
            popover.set_parent(&new_tab_wrapper_ref);
            
            let box_container = gtk::Box::new(gtk::Orientation::Vertical, 5);
            box_container.set_margin_top(5);
            box_container.set_margin_bottom(5);
            box_container.set_margin_start(5);
            box_container.set_margin_end(5);
            
            // Close tab option
            let close_item = gtk::Button::new();
            close_item.set_label("Close Tab");
            close_item.set_css_classes(&["menu-item"]);
            close_item.set_has_frame(false);
            
            // Create fresh clones for this inner closure
            let tabs_box_for_close = tabs_box_ref_clone.clone();
            let new_tab_wrapper_for_close = new_tab_wrapper_ref.clone();
            let text_view_for_close = text_view_ref_clone.clone();
            let buffer_for_close = buffer_for_menu.clone();
            let tab_button_wrapper_for_close = tab_button_wrapper_ref_clone.clone();
            let popover_for_close = popover.clone();
            
            let close_item_clone = close_item.clone();
            close_item.connect_clicked(move |_| {
                // Check if this is the active tab
                let is_active = new_tab_wrapper_for_close.css_classes().iter().any(|class| class == "active");
                
                // Remove this tab
                tabs_box_for_close.remove(&new_tab_wrapper_for_close);
                
                // If this was the active tab, switch back to the first tab
                if is_active {
                    text_view_for_close.set_buffer(Some(&buffer_for_close));
                    tab_button_wrapper_for_close.set_css_classes(&["tab-button-wrapper", "active"]);
                }
                
                // Close the popover
                popover_for_close.popdown();
            });
            
            // Clear tab content option
            let clear_item = gtk::Button::new();
            clear_item.set_label("Clear Content");
            clear_item.set_css_classes(&["menu-item"]);
            clear_item.set_has_frame(false);
            
            // Create fresh clone for this inner closure
            let new_buffer_clear = new_buffer_for_menu.clone();
            let popover_clear = popover.clone();
            
            let clear_item_clone = clear_item.clone();
            clear_item.connect_clicked(move |_| {
                new_buffer_clear.set_text("");
                popover_clear.popdown();
            });
            
            box_container.append(&close_item_clone);
            box_container.append(&clear_item_clone);
            
            popover.set_child(Some(&box_container));
            popover.popup();
        });
        
        new_tab_wrapper.add_controller(right_click);
        
        // Move the + button to the end
        tabs_box_ref.remove(&new_tab_button_ref);
        tabs_box_ref.append(&new_tab_wrapper);
        tabs_box_ref.append(&new_tab_button_ref);
        
        // Simulate a click on the new tab to activate it
        new_tab_wrapper.emit_clicked();
    });
    
    // Make the close button for the first tab work
    let buffer_clone = buffer.clone();
    
    close_icon.connect_clicked(move |_| {
        // Just clear the content of this tab
        buffer_clone.set_text("");
    });
    
    // Connect the initial tab to activate it when clicked
    let text_view_ref = text_view.clone();
    let buffer_clone = buffer.clone();
    
    tab_button_wrapper.connect_clicked(move |clicked_button| {
        // Set this tab as active
        clicked_button.set_css_classes(&["tab-button-wrapper", "active"]);
        
        // Switch to this tab's buffer
        text_view_ref.set_buffer(Some(&buffer_clone));
    });
    
    // Create tabs container with tabs and add button
    tabs_container.append(&tabs_box);
    
    // Add tabs container to tabs row
    tabs_row.append(&tabs_container);
    
    // Add the tabs row to the main container
    main_container.append(&tabs_row);

    // Return the main container, button references, and find/replace buttons
    (main_container, new_button_wrapper, open_button_wrapper, save_button_wrapper.clone(), open_recent_wrapper, save_as_button_wrapper, tabs_box, find_button, replace_button, show_line_numbers_button)
}

fn update_status_bar(status_label: &gtk::Label, buffer: &gtk::TextBuffer, editor_state: &Arc<Mutex<EditorState>>) {
    if let Ok(state) = editor_state.lock() {
        let modified = state.is_modified;
        let (line, column) = get_cursor_position(buffer);
        
        let modified_marker = if modified { "*" } else { "" };
        status_label.set_text(&format!("{}Line: {} Col: {}", modified_marker, line, column));
    }
}

fn get_cursor_position(buffer: &gtk::TextBuffer) -> (u32, u32) {
    if let Some(mark) = buffer.mark("insert") {
        let iter = buffer.iter_at_mark(&mark);
        return ((iter.line() + 1) as u32, (iter.line_offset() + 1) as u32);
    }
    (1, 1)
}

fn apply_syntax_highlighting(buffer: &gtk::TextBuffer) {
    // Clear existing tags
    buffer.remove_all_tags(&buffer.start_iter(), &buffer.end_iter());
    
    let text = buffer.text(&buffer.start_iter(), &buffer.end_iter(), false);
    let content = text.as_str();
    
    // Rust keywords
    let keywords = [
        "as", "break", "const", "continue", "crate", "else", "enum", "extern",
        "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod",
        "move", "mut", "pub", "ref", "return", "self", "Self", "static", "struct",
        "super", "trait", "true", "type", "unsafe", "use", "where", "while", "async",
        "await", "dyn", "abstract", "become", "box", "do", "final", "macro", "override",
        "priv", "typeof", "unsized", "virtual", "yield"
    ];
    
    // Rust types
    let types = [
        "bool", "char", "f32", "f64", "i8", "i16", "i32", "i64", "i128", "isize",
        "u8", "u16", "u32", "u64", "u128", "usize", "str", "String", "Vec"
    ];
    
    // Apply keyword highlighting
    for keyword in keywords {
        let mut start_search = buffer.start_iter();
        while let Some((match_start, match_end)) = start_search.forward_search(
            keyword,
            gtk::TextSearchFlags::CASE_INSENSITIVE,
            None,
        ) {
            // Only highlight if it's a whole word
            if is_word_boundary(&match_start, true) && is_word_boundary(&match_end, false) {
                buffer.apply_tag_by_name("keyword", &match_start, &match_end);
            }
            start_search = match_end;
        }
    }
    
    // Apply type highlighting
    for type_name in types {
        let mut start_search = buffer.start_iter();
        while let Some((match_start, match_end)) = start_search.forward_search(
            type_name,
            gtk::TextSearchFlags::CASE_INSENSITIVE,
            None,
        ) {
            // Only highlight if it's a whole word
            if is_word_boundary(&match_start, true) && is_word_boundary(&match_end, false) {
                buffer.apply_tag_by_name("type", &match_start, &match_end);
            }
            start_search = match_end;
        }
    }
    
    // Highlight strings
    let mut in_string = false;
    let mut string_start = buffer.start_iter();
    
    let mut start_search = buffer.start_iter();
    while !start_search.is_end() {
        let ch = start_search.char();
        
        if ch == '"' && (!in_string || start_search.backward_char() && start_search.char() != '\\') {
            start_search.forward_char();
            if !in_string {
                string_start = start_search.clone();
                in_string = true;
            } else {
                buffer.apply_tag_by_name("string", &string_start, &start_search);
                in_string = false;
            }
        } else {
            start_search.forward_char();
        }
    }
    
    // Highlight comments (// and /* */)
    let mut start_search = buffer.start_iter();
    while let Some((comment_start, _)) = start_search.forward_search(
        "//",
        gtk::TextSearchFlags::CASE_INSENSITIVE,
        None,
    ) {
        let mut line_end = comment_start.clone();
        line_end.forward_to_line_end();
        
        buffer.apply_tag_by_name("comment", &comment_start, &line_end);
        start_search = line_end;
    }
    
    // Block comments /* */
    let mut start_search = buffer.start_iter();
    while let Some((block_start, _)) = start_search.forward_search(
        "/*",
        gtk::TextSearchFlags::CASE_INSENSITIVE,
        None,
    ) {
        if let Some((block_end, _)) = block_start.forward_search(
            "*/",
            gtk::TextSearchFlags::CASE_INSENSITIVE,
            None,
        ) {
            buffer.apply_tag_by_name("comment", &block_start, &block_end);
            start_search = block_end;
        } else {
            break;
        }
    }
    
    // Detect simple syntax errors
    check_for_errors(buffer, content);
}

fn is_word_boundary(iter: &gtk::TextIter, is_start: bool) -> bool {
    if is_start {
        iter.starts_word() || iter.starts_line() || {
            let mut temp = iter.clone();
            if temp.backward_char() {
                !temp.char().is_alphanumeric()
            } else {
                true
            }
        }
    } else {
        iter.ends_word() || iter.ends_line() || !iter.char().is_alphanumeric()
    }
}

fn check_for_errors(buffer: &gtk::TextBuffer, content: &str) {
    // Pattern for unmatched brackets/parentheses
    let brackets: Vec<(char, char)> = vec![
        ('(', ')'),
        ('{', '}'),
        ('[', ']'),
    ];
    
    // Check for unmatched brackets
    for (open_bracket, close_bracket) in brackets {
        let mut stack: Vec<(usize, usize)> = Vec::new();  // (line, col) positions
        let mut line = 0;
        let mut col = 0;
        
        for ch in content.chars() {
            if ch == '\n' {
                line += 1;
                col = 0;
            } else {
                col += 1;
                
                if ch == open_bracket {
                    stack.push((line, col));
                } else if ch == close_bracket {
                    if stack.is_empty() {
                        // Unmatched closing bracket
                        highlight_error_at_position(buffer, line, col);
                    } else {
                        stack.pop();
                    }
                }
            }
        }
        
        // Unmatched opening brackets
        for (line, col) in stack {
            highlight_error_at_position(buffer, line, col);
        }
    }
    
    // Check for missing semicolons
    for (line_idx, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if !trimmed.is_empty() && 
           !trimmed.ends_with(';') && 
           !trimmed.ends_with('{') && 
           !trimmed.ends_with('}') && 
           !trimmed.starts_with("//") &&
           !trimmed.starts_with("pub fn") &&
           !trimmed.starts_with("fn") &&
           !trimmed.contains("->") {
            // Potential missing semicolon
            if let Some(iter) = buffer.iter_at_line_offset(line_idx as i32, 0) {
                let mut end = iter.clone();
                if end.forward_to_line_end() {
                    // Skip if it's inside a comment or string
                    let text = buffer.text(&iter, &end, false);
                    if !text.contains("//") && !text.contains("/*") && !is_inside_string(&text) {
                        buffer.apply_tag_by_name("error", &iter, &end);
                    }
                }
            }
        }
    }
}

fn is_inside_string(text: &str) -> bool {
    let mut in_string = false;
    let mut escaped = false;
    
    for ch in text.chars() {
        if ch == '\\' {
            escaped = !escaped;
        } else if ch == '"' && !escaped {
            in_string = !in_string;
        } else {
            escaped = false;
        }
    }
    
    in_string
}

fn highlight_error_at_position(buffer: &gtk::TextBuffer, line: usize, col: usize) {
    if let Some(iter) = buffer.iter_at_line_offset(line as i32, 0) {
        let mut pos = iter.clone();
        if pos.forward_chars(col as i32) {
            let mut end = pos.clone();
            if end.forward_char() {
                buffer.apply_tag_by_name("error", &pos, &end);
            }
        }
    }
}

fn apply_zoom(text_view: &gtk::TextView, zoom_level: f64) {
    let provider = gtk::CssProvider::new();
    let css = format!(
        "textview {{ font-family: 'Monospace'; font-size: {}px; line-height: 1.4; }}",
        (13.0 * zoom_level).round()
    );
    
    provider.load_from_data(&css);
    
    let context = text_view.style_context();
    context.add_provider(&provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
}

// In the beginning of the main function or after TextBuffer creation
fn highlight_current_line(buffer: &gtk::TextBuffer, _text_view: &gtk::TextView) {
    // Create provider for current line highlight
    let provider = gtk::CssProvider::new();
    provider.load_from_data(".line-highlight { background-color: rgba(255, 255, 255, 0.04); }");
    
    let display = gtk::gdk::Display::default().unwrap();
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
    
    // Get the tag table
    let tag_table = buffer.tag_table();
    
    // Create tag for line highlight if needed
    if tag_table.lookup("line-highlight").is_none() {
        let tag = gtk::TextTag::builder()
            .name("line-highlight")
            .background_rgba(&gtk::gdk::RGBA::new(0.15, 0.15, 0.15, 1.0))
            .build();
        tag_table.add(&tag);
    }
    
    // Update highlight when cursor moves
    let buffer_clone_highlight = buffer.clone();
    buffer.connect_mark_set(move |buffer, iter, mark| {
        if let Some(mark_name) = mark.name() {
            if mark_name == "insert" {
                update_highlight_line(buffer, iter);
            }
        }
    });
    
    // Initial highlight
    if let Some(mark) = buffer.mark("insert") {
        let iter = buffer.iter_at_mark(&mark);
        update_highlight_line(&buffer_clone_highlight, &iter);
    }
}

fn update_highlight_line(buffer: &gtk::TextBuffer, iter: &gtk::TextIter) {
    // Remove previous highlight
    let start = buffer.start_iter();
    let end = buffer.end_iter();
    buffer.remove_tag_by_name("line-highlight", &start, &end);
    
    // Get line bounds
    let mut line_start = iter.clone();
    line_start.set_line_offset(0);
    let mut line_end = line_start.clone();
    line_end.forward_to_line_end();
    
    // Apply highlight
    buffer.apply_tag_by_name("line-highlight", &line_start, &line_end);
}

fn main() -> Result<()> {
    // Force Wayland backend for GTK
    env::set_var("GDK_BACKEND", "wayland");
    
    env_logger::init();
    info!("Starting application with GTK");

    // Initialize GTK
    gtk::init().expect("Failed to initialize GTK");

    let app = gtk::Application::builder()
        .application_id("com.example.rustedit")
        .build();

    let editor_state = Arc::new(Mutex::new(EditorState::new()));

    app.connect_activate(move |app| {
        debug!("Application activated");
        
        // Create GTK window and text view first
        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .title("RustEdit")
            .default_width(1280)
            .default_height(720)
            .css_classes(["dark"])
            .build();

        // Set proper visual appearance
        window.add_css_class("dark");
        
        // Create a GTK box to hold our content
        let vbox = gtk::Box::new(gtk::Orientation::Vertical, 0);
        window.set_child(Some(&vbox));
        
        // Create text buffer with syntax highlighting
        let tag_table = create_tag_table();
        let buffer = TextBuffer::new(Some(&tag_table));
        
        // Create status bar
        let status_bar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        status_bar.set_margin_start(8);
        status_bar.set_margin_end(8);
        status_bar.set_margin_top(4);
        status_bar.set_margin_bottom(4);
        status_bar.set_css_classes(&["status-bar"]);
        
        let status_label = gtk::Label::new(Some("Line: 1 Col: 1"));
        status_label.set_halign(gtk::Align::Start);
        status_label.set_css_classes(&["status-label"]);
        status_bar.append(&status_label);
        
        // Create scroll window for text view
        let scroll = gtk::ScrolledWindow::new();
        scroll.set_vexpand(true);
        scroll.set_hexpand(true);
        scroll.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Automatic);
        scroll.set_overlay_scrolling(true);
        scroll.set_css_classes(&["editor-scroll"]);
        
        // Create text view with better styling
        let text_view = gtk::TextView::with_buffer(&buffer);
        text_view.set_monospace(true);
        text_view.set_wrap_mode(gtk::WrapMode::None);
        text_view.set_left_margin(10);
        text_view.set_right_margin(10);
        text_view.set_top_margin(10);
        text_view.set_bottom_margin(10);
        text_view.set_cursor_visible(true);
        text_view.set_editable(true);
        text_view.set_pixels_above_lines(2);
        text_view.set_pixels_below_lines(2);
        text_view.set_pixels_inside_wrap(0);
        text_view.set_hexpand(true);
        text_view.set_vexpand(true);
        
        // Set dark mode for the text view
        text_view.set_css_classes(&["dark-mode"]);
        
        // Create menu bar and add it to the vbox - note that menu_bar is now the main_container with both menu and tabs
        let (menu_container, new_button, open_button, save_button, _open_recent_button, save_as_button, _tabs_box, find_button, replace_button, show_line_numbers_button) = 
            create_menu_bar(&window, &buffer, editor_state.clone(), status_label.clone(), &text_view);
        vbox.append(&menu_container);
        
        // Set up find and replace button handlers now that text_view is available
        let buffer_ref = buffer.clone();
        let window_ref = window.clone();
        let text_view_ref = text_view.clone();
        
        // Set up current line highlighting
        let buffer_for_highlight = buffer.clone();
        let text_view_for_highlight = text_view.clone();
        highlight_current_line(&buffer_for_highlight, &text_view_for_highlight);
        
        find_button.connect_clicked(move |_| {
            // Create a dialog for find
            let dialog = gtk::Dialog::with_buttons(
                Some("Find"),
                Some(&window_ref),
                gtk::DialogFlags::MODAL | gtk::DialogFlags::DESTROY_WITH_PARENT,
                &[
                    ("Find", gtk::ResponseType::Accept),
                    ("Cancel", gtk::ResponseType::Cancel),
                ],
            );
            dialog.set_default_width(350);
            
            // Create the content area
            let content_area = dialog.content_area();
            
            let grid = gtk::Grid::new();
            grid.set_row_spacing(6);
            grid.set_column_spacing(6);
            grid.set_margin_start(10);
            grid.set_margin_end(10);
            grid.set_margin_top(10);
            grid.set_margin_bottom(10);
            
            let find_label = gtk::Label::new(Some("Find what:"));
            find_label.set_halign(gtk::Align::Start);
            
            let find_entry = gtk::Entry::new();
            find_entry.set_hexpand(true);
            
            grid.attach(&find_label, 0, 0, 1, 1);
            grid.attach(&find_entry, 1, 0, 1, 1);
            
            content_area.append(&grid);
            dialog.show();
            
            // Get the buffer for searching
            let buffer = buffer_ref.clone();
            let text_view = text_view_ref.clone();
            
            dialog.connect_response(move |dialog, response| {
                if response == gtk::ResponseType::Accept {
                    let search_text = find_entry.text();
                    if !search_text.is_empty() {
                        // Get the cursor position or start of buffer
                        let mut start_iter = buffer.start_iter();
                        if let Some(mark) = buffer.mark("insert") {
                            start_iter = buffer.iter_at_mark(&mark);
                        }
                        
                        // Search for text
                        if let Some((match_start, match_end)) = start_iter.forward_search(
                            &search_text,
                            gtk::TextSearchFlags::CASE_INSENSITIVE,
                            None,
                        ) {
                            // Select the found text
                            buffer.select_range(&match_start, &match_end);
                            
                            // Scroll to the selection
                            if let Some(mark) = buffer.mark("insert") {
                                text_view.scroll_to_mark(&mark, 0.1, false, 0.0, 0.5);
                            }
                        }
                    }
                }
                dialog.destroy();
            });
        });
        
        let buffer_ref = buffer.clone();
        let window_ref = window.clone();
        let text_view_ref = text_view.clone();
        
        replace_button.connect_clicked(move |_| {
            // Create a dialog for replace
            let dialog = gtk::Dialog::with_buttons(
                Some("Replace"),
                Some(&window_ref),
                gtk::DialogFlags::MODAL | gtk::DialogFlags::DESTROY_WITH_PARENT,
                &[
                    ("Replace", gtk::ResponseType::Accept),
                    ("Replace All", gtk::ResponseType::Apply),
                    ("Cancel", gtk::ResponseType::Cancel),
                ],
            );
            dialog.set_default_width(350);
            
            // Create the content area
            let content_area = dialog.content_area();
            
            let grid = gtk::Grid::new();
            grid.set_row_spacing(6);
            grid.set_column_spacing(6);
            grid.set_margin_start(10);
            grid.set_margin_end(10);
            grid.set_margin_top(10);
            grid.set_margin_bottom(10);
            
            let find_label = gtk::Label::new(Some("Find what:"));
            find_label.set_halign(gtk::Align::Start);
            
            let find_entry = gtk::Entry::new();
            find_entry.set_hexpand(true);
            
            let replace_label = gtk::Label::new(Some("Replace with:"));
            replace_label.set_halign(gtk::Align::Start);
            
            let replace_entry = gtk::Entry::new();
            replace_entry.set_hexpand(true);
            
            grid.attach(&find_label, 0, 0, 1, 1);
            grid.attach(&find_entry, 1, 0, 1, 1);
            grid.attach(&replace_label, 0, 1, 1, 1);
            grid.attach(&replace_entry, 1, 1, 1, 1);
            
            content_area.append(&grid);
            dialog.show();
            
            // Get the buffer for searching and replacing
            let buffer = buffer_ref.clone();
            let text_view = text_view_ref.clone();
            let window_ref = window_ref.clone();
            
            dialog.connect_response(move |dialog, response| {
                let search_text = find_entry.text();
                let replace_text = replace_entry.text();
                
                if response == gtk::ResponseType::Accept && !search_text.is_empty() {
                    // Get the cursor position or start of buffer
                    let mut start_iter = buffer.start_iter();
                    if let Some(mark) = buffer.mark("insert") {
                        start_iter = buffer.iter_at_mark(&mark);
                    }
                    
                    // Search for text
                    if let Some((mut match_start, mut match_end)) = start_iter.forward_search(
                        &search_text,
                        gtk::TextSearchFlags::CASE_INSENSITIVE,
                        None,
                    ) {
                        // Replace the found text
                        buffer.begin_user_action();
                        buffer.delete(&mut match_start, &mut match_end);
                        buffer.insert(&mut match_start, &replace_text);
                        buffer.end_user_action();
                        
                        // Move cursor to the end of the replaced text
                        buffer.place_cursor(&match_start);
                        
                        // Scroll to the replaced text
                        if let Some(mark) = buffer.mark("insert") {
                            text_view.scroll_to_mark(&mark, 0.1, false, 0.0, 0.5);
                        }
                    }
                } else if response == gtk::ResponseType::Apply && !search_text.is_empty() {
                    // Replace all occurrences
                    let mut start_iter = buffer.start_iter();
                    let mut count = 0;
                    
                    buffer.begin_user_action();
                    while let Some((mut match_start, mut match_end)) = start_iter.forward_search(
                        &search_text,
                        gtk::TextSearchFlags::CASE_INSENSITIVE,
                        None,
                    ) {
                        // Replace the found text
                        buffer.delete(&mut match_start, &mut match_end);
                        buffer.insert(&mut match_start, &replace_text);
                        
                        // Move start_iter to continue searching
                        start_iter = match_start;
                        count += 1;
                    }
                    buffer.end_user_action();
                    
                    let window_ref_local = window_ref.clone();
                    // Show a message about how many replacements were made
                    let message = gtk::MessageDialog::new(
                        Some(&window_ref_local),
                        gtk::DialogFlags::MODAL | gtk::DialogFlags::DESTROY_WITH_PARENT,
                        gtk::MessageType::Info,
                        gtk::ButtonsType::Ok,
                        &format!("Replaced {} occurrences", count),
                    );
                    message.connect_response(|dialog, _| {
                        dialog.destroy();
                    });
                    message.show();
                }
                
                if response != gtk::ResponseType::Apply {
                    dialog.destroy();
                }
            });
        });
        
        // Apply CSS to ensure dark styling
        let provider = gtk::CssProvider::new();
        provider.load_from_data(
            "
            window {
                background-color: #1e1e1e;
            }
            headerbar {
                background-color: #1e1e1e;
                border-bottom: none;
                padding: 0;
                min-height: 0;
            }
            headerbar button {
                margin: 0;
                padding: 2px;
                background: none;
                border: none;
                color: #e0e0e0;
            }
            headerbar button:hover {
                background-color: rgba(255, 255, 255, 0.1);
            }
            .dark-mode {
                background-color: #1e1e1e;
                color: #e0e0e0;
                caret-color: #ffffff;
            }
            .line-numbers {
                background-color: #1e1e1e;
                color: #707070;
                border-right: 1px solid #303030;
                margin: 0;
                padding: 6px 0 0 0;
            }
            .text-box {
                background-color: #1e1e1e;
                margin: 0;
                padding: 0;
            }
            textview {
                font-family: 'Monospace';
                font-size: 12px;
                padding: 0;
                background-color: #1e1e1e;
            }
            textview text {
                background-color: #1e1e1e;
                color: #e0e0e0;
            }
            scrolledwindow {
                border: none;
                background-color: #1e1e1e;
                padding: 0;
                margin: 0;
            }
            .error-line {
                background-color: rgba(255, 0, 0, 0.2);
            }
            .error-text {
                text-decoration: underline;
                text-decoration-color: #ff3333;
                text-decoration-style: wavy;
            }
            .main-menu-container {
                background-color: #1e1e1e;
            }
            .menu-bar {
                background-color: #1e1e1e;
                padding: 0 4px;
                border-bottom: none;
            }
            .menu-button {
                background: none;
                color: #e0e0e0;
                margin-right: 1px;
                margin-top: 0;
                margin-bottom: 0;
                font-size: 0.95em;
                min-height: 18px;
                padding: 1px 1px;
                border: none;
                border-radius: 2px;
                box-shadow: none;
                outline: none;
                font-weight: normal;
                width: min-content;
                min-width: min-content;
            }
            .menu-button:hover {
                background-color: rgba(255, 255, 255, 0.05);
            }
            .menu-button:active, 
            .menu-button:checked,
            .menu-button:focus {
                outline: none;
                box-shadow: none;
                background-color: rgba(255, 255, 255, 0.05);
            }
            menubutton {
                padding: 0;
                margin: 0;
                min-height: 0;
                min-width: 0;
                width: min-content;
                outline: none;
                box-shadow: none;
                background: none;
            }
            menubutton > box {
                min-height: 0;
                padding: 0;
                margin: 0;
                width: min-content;
            }
            menubutton:focus, menubutton:active {
                outline: none;
                box-shadow: none;
            }
            menubutton > arrow {
                -gtk-icon-size: 0;
                min-height: 0;
                min-width: 0;
                padding: 0;
                margin: 0;
                opacity: 0;
            }
            menubutton button {
                border: none !important;
                outline: none !important;
                box-shadow: none !important;
                background: none !important;
            }
            
            menubutton > button:focus,
            menubutton > button:active,
            menubutton > button:checked {
                outline: none !important;
                border: none !important;
                box-shadow: none !important;
            }
            .text-button {
                background: none;
                color: #e0e0e0;
                margin-right: 12px;
                margin-top: 2px;
                margin-bottom: 2px;
                font-size: 0.95em;
                min-height: 18px;
                padding: 2px 8px;
                border: 1px solid rgba(255, 255, 255, 0.15);
                border-radius: 4px;
                box-shadow: none;
            }
            .text-button:hover {
                background-color: rgba(255, 255, 255, 0.05);
                border-color: rgba(255, 255, 255, 0.2);
            }
            .text-button:active, 
            .text-button:checked,
            .text-button:focus {
                background-color: rgba(255, 255, 255, 0.05);
                border-color: rgba(255, 255, 255, 0.2);
                box-shadow: none;
                outline: none;
            }
            .menu-separator {
                margin: 0;
                background-color: #303030;
            }
            .shortcut-label {
                opacity: 0.7;
                font-size: 0.9em;
            }
            .tabs-row {
                background-color: #1e1e1e;
                padding: 1px 0 1px 35px; 
                border-bottom: 1px solid #202020;
            }
            .tab-bar {
                background-color: #1e1e1e;
                padding: 0;
            }
            .tabs-box {
                padding: 0;
            }
            .tab-button {
                background-color: #252525;
                padding: 2px 6px;
                border-radius: 2px;
                margin-right: 1px;
                border: none;
                color: #d0d0d0;
                min-width: 0;
                width: auto;
                transition: background-color 150ms ease-out;
            }
            .tab-button-wrapper {
                background: none;
                border-radius: 2px;
                margin: 0 1px 0 0;
                min-height: 0;
                min-width: 0;
                width: auto;
                transition: all 150ms ease-out;
            }
            .tab-button-wrapper:checked .tab-button,
            .tab-button-wrapper:active .tab-button {
                background-color: #303030;
                box-shadow: none;
            }
            .tab-label {
                color: #e0e0e0;
                font-size: 0.95em;
                padding: 0;
                margin: 0;
                min-width: 0;
                width: auto;
            }
            .tab-close-button {
                padding: 0;
                min-height: 12px;
                min-width: 12px;
                border-radius: 2px;
                background: none;
                opacity: 0.7;
                transition: all 150ms ease-out;
            }
            .tab-close-button:hover {
                background-color: rgba(255, 0, 0, 0.2);
                opacity: 1;
            }
            .new-tab-button {
                padding: 2px;
                min-height: 20px;
                min-width: 20px;
                margin: 1px 2px 0 4px;
                border-radius: 3px;
                background: rgba(255, 255, 255, 0.03);
                color: #d0d0d0;
                border: none;
                position: relative;
                top: 1px;
                transition: all 150ms ease-out;
            }
            .new-tab-button:hover {
                background-color: rgba(255, 255, 255, 0.08);
            }
            .tab-button-wrapper.active .tab-button {
                background-color: #3a3a3a;
                box-shadow: none;
                transition: background-color 150ms ease-out;
            }
            .tab-button-wrapper.active {
                background-color: transparent;
                transition: all 150ms ease-out;
            }
            button {
                min-height: 0;
                min-width: 0;
            }
            popover, 
            popover contents {
                background-color: #252525;
                border: none;
                border-radius: 3px;
                box-shadow: 0 3px 6px rgba(0, 0, 0, 0.4);
                margin: 0;
                padding: 1px;
            }
            popover box {
                padding: 0;
                margin: 0;
                spacing: 2px;
            }
            popover button {
                border: none;
                background: none;
                box-shadow: none;
                outline: none;
                padding: 3px 6px;
                color: #e0e0e0;
                min-height: 24px;
                min-width: 0;
                width: auto;
                border-radius: 4px;
            }
            
            popover button:not(:hover) {
                background-color: transparent;
            }
            
            popover button:hover {
                background-color: rgba(255, 255, 255, 0.1);
            }
            
            popover.menu {
                padding: 0;
                margin: 0;
            }
            .status-bar {
                background-color: #252525;
                border-top: 1px solid rgba(255, 255, 255, 0.1);
                padding: 2px 8px;
            }
            .status-label {
                color: #b0b0b0;
                font-size: 0.9em;
            }
            .tab-button-wrapper.active .tab-button {
                background-color: #3a3a3a;
                box-shadow: none;
            }
            .tab-button-wrapper.active {
                background-color: transparent;
            }
            "
        );
        
        let display = gtk::gdk::Display::default().unwrap();
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );

        // Create a box for text view and line numbers with better layout
        let text_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        text_box.set_hexpand(true);
        text_box.set_vexpand(true);
        text_box.set_css_classes(&["text-box"]);

        // Create line number display
        let line_numbers = gtk::DrawingArea::new();
        line_numbers.set_width_request(30);
        line_numbers.set_hexpand(false);
        line_numbers.set_vexpand(true);
        line_numbers.set_content_width(30);

        // Add a CSS class for styling the line numbers
        line_numbers.set_css_classes(&["line-numbers"]);

        // Set reference to buffer for drawing line numbers
        let buffer_for_draw = buffer.clone();
        let text_view_for_draw = text_view.clone();

        // Set up the drawing function for line numbers
        line_numbers.set_draw_func(move |_, cr, width, height| {
            // Set dark background for line numbers
            cr.set_source_rgb(0.12, 0.12, 0.12);  // Darker background to match theme
            cr.rectangle(0.0, 0.0, width as f64, height as f64);
            cr.fill().expect("Failed to fill background");
            
            // Use light gray text for line numbers
            cr.set_source_rgb(0.5, 0.5, 0.5);  // More subtle color for line numbers
            
            let layout = pangocairo::functions::create_layout(cr);
            let font_desc = pango::FontDescription::from_string("Monospace 9");
            layout.set_font_description(Some(&font_desc));
            
            // Get visible range and adjustment values
            let vadj = text_view_for_draw.vadjustment().unwrap();
            let scroll_pos = vadj.value();
            let line_height = 18.0; // Approximate line height
            
            // Calculate first visible line
            let start_line = (scroll_pos / line_height).floor() as i32;
            let visible_lines = (height as f64 / line_height).ceil() as i32 + 1;
            let line_count = buffer_for_draw.line_count();
            
            // Draw visible line numbers
            for i in 0..visible_lines {
                let line_num = start_line + i;
                if line_num < line_count {
                    // Calculate y position with offset for scrolling
                    let y = (i as f64 * line_height) - (scroll_pos % line_height);
                    
                    layout.set_text(&format!("{:>3}", line_num + 1));
                    cr.move_to(4.0, y);  // Added a bit more padding
                    pangocairo::functions::show_layout(cr, &layout);
                }
            }
        });

        // Handle adjustments to redraw line numbers when scrolling
        if let Some(vadj) = text_view.vadjustment() {
            let line_numbers_clone = line_numbers.clone();
            vadj.connect_value_changed(move |_| {
                line_numbers_clone.queue_draw();
            });
        }

        // Create text source view with line numbers
        text_box.append(&line_numbers);
        text_box.append(&text_view);
        
        // Add the text box to the scroll window
        scroll.set_child(Some(&text_box));
        
        // Ensure the scroll window is added to the vbox
        vbox.append(&scroll);

        // Add status bar to vbox
        vbox.append(&status_bar);
        
        // Update status bar when cursor position changes
        let state_ref = editor_state.clone();
        let status_label_ref = status_label.clone();
        buffer.connect_changed(move |buf| {
            let text = buf.text(&buf.start_iter(), &buf.end_iter(), false);
            let text_str = text.as_str();
            
            if let Ok(mut state) = state_ref.lock() {
                state.is_modified = true;
                
                // Only push to undo stack if content actually changed
                if state.text_buffer.text() != text_str {
                    // Store current text before modifying it
                    let current_text = state.text_buffer.text().to_string();
                    state.push_to_undo_stack(&current_text);
                    state.text_buffer.set_text(text_str);
                }
            }
            update_status_bar(&status_label_ref, buf, &state_ref);
            
            // Apply syntax highlighting
            apply_syntax_highlighting(buf);
        });
        
        let state_ref = editor_state.clone();
        let status_label_ref = status_label.clone();
        buffer.connect_mark_set(move |buf, _, _| {
            update_status_bar(&status_label_ref, buf, &state_ref);
        });
        
        // Set up keyboard shortcuts with additional zoom functionality
        let key_controller = gtk::EventControllerKey::new();
        let save_button_ref = save_button;
        let open_button_ref = open_button;
        let new_button_ref = new_button;
        let save_as_button_ref = save_as_button;
        let state_ref = editor_state.clone();
        let text_view_ref = text_view.clone();
        let window_ref = window.clone();  // Create a separate clone for the closure
        
        key_controller.connect_key_pressed(move |_, key, _keycode, state| {
            let ctrl = state.contains(gtk::gdk::ModifierType::CONTROL_MASK);
            let shift = state.contains(gtk::gdk::ModifierType::SHIFT_MASK);
            
            if ctrl {
                match key {
                    gtk::gdk::Key::s => {
                        if shift {
                            // Ctrl+Shift+S - Save As
                            save_as_button_ref.emit_clicked();
                        } else {
                            // Ctrl+S - Save
                            save_button_ref.emit_clicked();
                        }
                        return glib::Propagation::Stop;
                    },
                    gtk::gdk::Key::o => {
                        // Ctrl+O - Open
                        open_button_ref.emit_clicked();
                        return glib::Propagation::Stop;
                    },
                    gtk::gdk::Key::t => {
                        // Ctrl+T - New File (changed from n to t to match COSMIC)
                        new_button_ref.emit_clicked();
                        return glib::Propagation::Stop;
                    },
                    gtk::gdk::Key::w => {
                        // Ctrl+W - Close File
                        buffer.set_text("");
                        if let Ok(mut state) = state_ref.lock() {
                            state.text_buffer.set_text("");
                            state.current_file = None;
                            state.is_modified = false;
                            state.update_tab_name();
                        }
                        return glib::Propagation::Stop;
                    },
                    gtk::gdk::Key::q => {
                        // Ctrl+Q - Quit
                        window_ref.close();  // Use window_ref instead of window
                        return glib::Propagation::Stop;
                    },
                    gtk::gdk::Key::plus | gtk::gdk::Key::equal => {
                        // Ctrl+Plus or Ctrl+= - Zoom In
                        if let Ok(mut state) = state_ref.lock() {
                            state.zoom_in();
                            apply_zoom(&text_view_ref, state.zoom_level);
                        }
                        return glib::Propagation::Stop;
                    },
                    gtk::gdk::Key::minus => {
                        // Ctrl+Minus - Zoom Out
                        if let Ok(mut state) = state_ref.lock() {
                            state.zoom_out();
                            apply_zoom(&text_view_ref, state.zoom_level);
                        }
                        return glib::Propagation::Stop;
                    },
                    gtk::gdk::Key::_0 => {
                        // Ctrl+0 - Reset Zoom
                        if let Ok(mut state) = state_ref.lock() {
                            state.reset_zoom();
                            apply_zoom(&text_view_ref, state.zoom_level);
                        }
                        return glib::Propagation::Stop;
                    },
                    gtk::gdk::Key::z => {
                        // Ctrl+Z - Undo
                        if let Ok(mut state) = state_ref.lock() {
                            if let Some(previous_text) = state.undo() {
                                buffer.set_text(&previous_text);
                                state.text_buffer.set_text(&previous_text);
                            }
                        }
                        return glib::Propagation::Stop;
                    },
                    gtk::gdk::Key::y => {
                        // Ctrl+Y - Redo
                        if let Ok(mut state) = state_ref.lock() {
                            if let Some(next_text) = state.redo() {
                                buffer.set_text(&next_text);
                                state.text_buffer.set_text(&next_text);
                            }
                        }
                        return glib::Propagation::Stop;
                    },
                    gtk::gdk::Key::f => {
                        // Ctrl+F - Find
                        find_button.emit_clicked();
                        return glib::Propagation::Stop;
                    },
                    gtk::gdk::Key::h => {
                        // Ctrl+H - Replace
                        replace_button.emit_clicked();
                        return glib::Propagation::Stop;
                    },
                    _ => {}
                }
            }
            
            glib::Propagation::Proceed
        });
        window.add_controller(key_controller);

        // Show the GTK window
        window.show();

        // Add this to the main function after creating text_view and line_numbers
        let line_numbers_ref = line_numbers.clone();
        show_line_numbers_button.connect_toggled(move |button| {
            if button.is_active() {
                line_numbers_ref.set_visible(true);
            } else {
                line_numbers_ref.set_visible(false);
            }
        });
    });

    app.run();
    Ok(())
}
