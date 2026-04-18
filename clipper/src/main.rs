use clipper::{ClipEntry, ClipperApp, Event, Key, Model};
use crux_core::Core;
use eframe::egui;
use parking_lot::Mutex;
use std::sync::Arc;

#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;

// chaOS color scheme
fn chaos_theme() -> egui::Visuals {
    let mut visuals = egui::Visuals::dark();

    let pink = egui::Color32::from_rgb(0xE6, 0x00, 0x7A);
    let bg = egui::Color32::from_rgba_unmultiplied(0, 0, 0, 0xCC);
    let bg_alt = egui::Color32::from_rgb(0x1A, 0x1B, 0x26);

    visuals.widgets.noninteractive.bg_fill = bg_alt;
    visuals.widgets.noninteractive.bg_stroke = egui::Stroke::new(2.0, pink);
    visuals.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0, pink);

    visuals.widgets.inactive.bg_fill = bg_alt;
    visuals.widgets.inactive.bg_stroke = egui::Stroke::new(2.0, pink);
    visuals.widgets.inactive.fg_stroke = egui::Stroke::new(1.0, pink);

    visuals.widgets.hovered.bg_fill = pink.linear_multiply(0.2);
    visuals.widgets.hovered.bg_stroke = egui::Stroke::new(2.0, pink);
    visuals.widgets.hovered.fg_stroke = egui::Stroke::new(1.0, pink);

    visuals.widgets.active.bg_fill = pink;
    visuals.widgets.active.bg_stroke = egui::Stroke::new(2.0, pink);
    visuals.widgets.active.fg_stroke = egui::Stroke::new(1.0, egui::Color32::BLACK);

    visuals.selection.bg_fill = pink;
    visuals.selection.stroke = egui::Stroke::new(1.0, egui::Color32::BLACK);

    visuals.window_fill = bg;
    visuals.window_stroke = egui::Stroke::new(3.0, pink);
    visuals.window_rounding = egui::Rounding::same(8.0);

    visuals.extreme_bg_color = bg;
    visuals.panel_fill = bg;
    visuals.faint_bg_color = bg_alt;

    visuals
}

mod daemon {
    use super::ClipEntry;
    use std::process::Command;

    pub fn ensure_daemon_running() -> Result<(), Box<dyn std::error::Error>> {
        match send_command("") {
            Ok(_) => Ok(()),
            Err(_) => {
                #[cfg(target_os = "windows")]
                {
                    use std::os::windows::process::CommandExt;
                    Command::new("nocb")
                        .arg("daemon")
                        .creation_flags(0x08000000) // CREATE_NO_WINDOW
                        .spawn()?;
                    }

                #[cfg(not(target_os = "windows"))]
                {
                    use std::process::Stdio;
                    Command::new("nocb")
                        .arg("daemon")
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .spawn()?;
                    }

                std::thread::sleep(std::time::Duration::from_millis(500));
                Ok(())
            }
        }
    }

    pub fn get_clips() -> Result<Vec<ClipEntry>, Box<dyn std::error::Error>> {
        // Use new search API for better data structure
        let output = Command::new("nocb").arg("search").output()?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut clips = Vec::new();

        for (id, line) in stdout.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }

            // Parse structured output: HASH:abc123|TYPE:text|SIZE:1K|TIME:5m|CONTENT:...
            let entry = parse_structured_entry(line, id)?;
            clips.push(entry);
        }

        Ok(clips)
    }

    fn parse_structured_entry(line: &str, id: usize) -> Result<ClipEntry, Box<dyn std::error::Error>> {
        let mut hash = String::new();
        let mut entry_type = String::new();
        let mut size_str: Option<String> = None;
        let mut time_ago = String::new();
        let mut content = String::new();

        // Split by pipe and parse each field
        for part in line.split('|') {
            if let Some(hash_val) = part.strip_prefix("HASH:") {
                hash = hash_val.to_string();
            } else if let Some(type_val) = part.strip_prefix("TYPE:") {
                entry_type = type_val.to_string();
            } else if let Some(size_val) = part.strip_prefix("SIZE:") {
                size_str = Some(size_val.to_string());
            } else if let Some(time_val) = part.strip_prefix("TIME:") {
                time_ago = time_val.to_string();
            } else if let Some(content_val) = part.strip_prefix("CONTENT:") {
                // Restore newlines from escaped format
                content = content_val.replace("\\n", "\n");
            }
        }

        // Fallback to old parsing if structured format not found
        if hash.is_empty() {
            return parse_legacy_entry(line, id);
        }

        Ok(ClipEntry {
            id: id as i64,
            hash,
            content,
            time_ago,
            entry_type,
            size_str,
        })
    }

    fn parse_legacy_entry(line: &str, id: usize) -> Result<ClipEntry, Box<dyn std::error::Error>> {
        // Fallback to old parsing logic for backward compatibility
        let parts: Vec<&str> = line.splitn(2, ' ').collect();
        if parts.len() < 2 {
            return Err("Invalid entry format".into());
        }

        let time_ago = parts[0].to_string();
        let rest = parts[1];

        let hash_pos = rest.rfind('#').unwrap_or(rest.len());
        let content = rest[..hash_pos].trim();
        let hash = if hash_pos < rest.len() {
            rest[hash_pos + 1..].trim().to_string()
        } else {
            format!("unknown{}", id)
        };

        let (entry_type, size_str, display_content) = if content.starts_with("[IMG:") {
            (
                "image".to_string(),
                parse_size_from_image(content),
                content.to_string(),
            )
        } else if content.contains(" [") && content.ends_with(']') {
            let bracket_pos = content.rfind(" [").unwrap_or(content.len());
            let text_part = &content[..bracket_pos];
            let size_part = &content[bracket_pos + 2..content.len() - 1];
            (
                "text".to_string(),
                Some(size_part.to_string()),
                text_part.to_string(),
            )
        } else {
            ("text".to_string(), None, content.to_string())
        };

        Ok(ClipEntry {
            id: id as i64,
            hash,
            content: display_content,
            time_ago,
            entry_type,
            size_str,
        })
    }

    fn parse_size_from_image(content: &str) -> Option<String> {
        if let Some(start) = content.find(' ') {
            if let Some(end) = content.rfind(' ') {
                if end > start {
                    return Some(content[end + 1..content.len() - 1].to_string());
                }
            }
        }
        None
    }

    pub fn send_command(selection: &str) -> Result<(), Box<dyn std::error::Error>> {
        Command::new("nocb").arg("copy").arg(selection).output()?;
        Ok(())
    }

}

struct ClipperGui {
    core: Core<ClipperApp>,
    model: Model,
    show_window: Arc<Mutex<bool>>,
}

impl ClipperGui {
    fn new(cc: &eframe::CreationContext<'_>, show_window: Arc<Mutex<bool>>) -> Self {
        cc.egui_ctx.set_visuals(chaos_theme());

        if let Err(e) = daemon::ensure_daemon_running() {
            eprintln!("Failed to start daemon: {}", e);
        }

        let mut app = Self {
            core: Core::new(),
            model: Model::default(),
            show_window,
        };

        app.process_event(Event::Init);
        app
    }

    fn process_event(&mut self, event: Event) {
        match &event {
            Event::Init | Event::RefreshClips | Event::LoadClips => {
                self.load_clips();
            }
            Event::CopyClip(index) => {
                if let Some(clip) = self.filtered_clips().get(*index) {
                    self.copy_to_clipboard(&clip);
                }
            }
            Event::CopyToClipboard(selection) => {
                if let Err(e) = daemon::send_command(selection) {
                    eprintln!("Failed to copy: {}", e);
                }
            }
            _ => {}
        }

        let _effects = self.core.process_event(event);
        self.model = self.core.view();
    }

    fn load_clips(&mut self) {
        match daemon::get_clips() {
            Ok(clips) => {
                self.process_event(Event::ClipsLoaded(clips));
            }
            Err(e) => {
                eprintln!("Failed to load clips: {}", e);
                self.process_event(Event::ClipsLoaded(vec![ClipEntry {
                    id: 0,
                    hash: "error".to_string(),
                    content: format!("Failed to load clips: {}", e),
                    time_ago: "!".to_string(),
                    entry_type: "text".to_string(),
                    size_str: None,
                }]));
            }
        }
    }

    fn copy_to_clipboard(&mut self, clip: &ClipEntry) {
        let selection = if clip.size_str.is_some() {
            format!(
                "{} {} [{}] #{}",
                clip.time_ago,
                clip.content,
                clip.size_str.as_ref().unwrap(),
                clip.hash
            )
        } else {
            format!("{} {} #{}", clip.time_ago, clip.content, clip.hash)
        };

        self.process_event(Event::CopyToClipboard(selection));
        self.process_event(Event::Copied);
    }

    fn filtered_clips(&self) -> Vec<ClipEntry> {
        if self.model.search_query.is_empty() {
            self.model.clips.clone()
        } else {
            let query = self.model.search_query.to_lowercase();
            self.model
                .clips
                .iter()
                .filter(|clip| clip.content.to_lowercase().contains(&query))
                .cloned()
                .collect()
        }
    }
}

impl eframe::App for ClipperGui {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if !*self.show_window.lock() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
            return;
        }

        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            *self.show_window.lock() = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        egui::CentralPanel::default()
            .frame(
                egui::Frame::none()
                .fill(egui::Color32::from_rgba_unmultiplied(0, 0, 0, 0xCC))
                .inner_margin(egui::Margin::same(24.0)),
            )
            .show(ctx, |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(8.0, 16.0);

                let bg_alt = egui::Color32::from_rgb(0x1A, 0x1B, 0x26);
                let pink = egui::Color32::from_rgb(0xE6, 0x00, 0x7A);
                let cyan = egui::Color32::from_rgb(0x00, 0xFF, 0xE1);

                egui::Frame::none()
                    .fill(bg_alt)
                    .rounding(egui::Rounding::same(5.0))
                    .inner_margin(egui::Margin::symmetric(16.0, 12.0))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.colored_label(pink, "󱞩 nocb:");

                            let response = ui.add_sized(
                                [ui.available_width() - 120.0, 20.0],
                                egui::TextEdit::singleline(&mut self.model.search_query)
                                .font(egui::TextStyle::Monospace)
                                .hint_text("Type to search...")
                                .desired_width(f32::INFINITY),
                            );

                            response.request_focus();

                            if response.changed() {
                                self.process_event(Event::UpdateSearch(
                                        self.model.search_query.clone(),
                                ));
                            }

                            ui.colored_label(
                                cyan,
                                format!(
                                    "{}/{}",
                                    self.filtered_clips().len(),
                                    self.model.clips.len()
                                ),
                            );

                            let search_icon = if self.model.full_search_mode { "🔍" } else { "📝" };
                            if ui.button(search_icon).on_hover_text("Toggle full-text search").clicked() {
                                self.process_event(Event::ToggleSearchMode);
                            }

                            if ui.button("⟳").clicked() {
                                self.process_event(Event::RefreshClips);
                            }
                        });
                    });

                ui.add_space(8.0);

                let clips = self.filtered_clips();
                egui::ScrollArea::vertical()
                    .auto_shrink([false; 2])
                    .show(ui, |ui| {
                        for (index, clip) in clips.iter().enumerate() {
                            let is_selected = index == self.model.selected_index;

                            let mut frame = egui::Frame::none()
                                .inner_margin(egui::Margin::symmetric(16.0, 10.0))
                                .rounding(egui::Rounding::same(5.0));

                            if is_selected {
                                frame = frame.fill(pink);
                            } else if index % 2 == 1 {
                                frame = frame.fill(egui::Color32::from_rgba_unmultiplied(
                                        0x1A, 0x1B, 0x26, 0x11,
                                ));
                            }

                            let response = frame
                                .show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        if is_selected {
                                            ui.colored_label(egui::Color32::BLACK, &clip.time_ago);
                                        } else {
                                            ui.colored_label(cyan, &clip.time_ago);
                                        }

                                        ui.separator();

                                        let label = if is_selected {
                                            ui.colored_label(egui::Color32::BLACK, &clip.content)
                                        } else {
                                            ui.label(&clip.content)
                                        };

                                        if let Some(size) = &clip.size_str {
                                            ui.with_layout(
                                                egui::Layout::right_to_left(egui::Align::Center),
                                                |ui| {
                                                    if is_selected {
                                                        ui.colored_label(
                                                            egui::Color32::BLACK,
                                                            size,
                                                        );
                                                    } else {
                                                        ui.colored_label(cyan, size);
                                                    }
                                                },
                                            );
                                        }

                                        label
                                    })
                                })
                            .inner;

                            if response.response.clicked() {
                                self.process_event(Event::SelectIndex(index));
                                self.process_event(Event::CopyClip(index));
                                *self.show_window.lock() = false;
                                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                            }

                            if response.response.hovered() && !is_selected {
                                self.process_event(Event::SelectIndex(index));
                            }
                        }
                    });

                if ctx.input(|i| i.key_pressed(egui::Key::ArrowUp)) {
                    self.process_event(Event::KeyPress(Key::Up));
                }
                if ctx.input(|i| i.key_pressed(egui::Key::ArrowDown)) {
                    self.process_event(Event::KeyPress(Key::Down));
                }
                if ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
                    self.process_event(Event::KeyPress(Key::Enter));
                    *self.show_window.lock() = false;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });

        ctx.request_repaint_after(std::time::Duration::from_secs(5));
    }
}

#[cfg(not(target_os = "linux"))]
fn setup_tray_and_hotkey(show_window: Arc<Mutex<bool>>) -> Result<(), Box<dyn std::error::Error>> {
    use global_hotkey::{
        hotkey::{Code, HotKey, Modifiers},
        GlobalHotKeyManager,
    };
    use tray_icon::{
        menu::{Menu, MenuItem},
        Icon, TrayIconBuilder,
    };

    let menu = Menu::new();
    let show_item = MenuItem::new("Show Clipper", true, None);
    let quit_item = MenuItem::new("Quit", true, None);

    menu.append(&show_item)?;
    menu.append(&quit_item)?;

    let show_id = show_item.id().clone();
    let quit_id = quit_item.id().clone();

    let mut icon_data = vec![0u8; 32 * 32 * 4];
    for chunk in icon_data.chunks_mut(4) {
        chunk[0] = 0xE6; // R
        chunk[1] = 0x00; // G
        chunk[2] = 0x7A; // B
        chunk[3] = 0xFF; // A
    }
    let icon = Icon::from_rgba(icon_data, 32, 32)?;

    let _tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("Clipper - Clipboard Manager")
        .with_icon(icon)
        .build()?;

    let manager = GlobalHotKeyManager::new()?;
    let hotkey = HotKey::new(
        Some(Modifiers::SUPER),
        Code::KeyB,
    );
    manager.register(hotkey)?;

    std::thread::spawn(move || {
        let menu_channel = tray_icon::menu::MenuEvent::receiver();
        let hotkey_channel = global_hotkey::GlobalHotKeyEvent::receiver();

        loop {
            if let Ok(event) = menu_channel.try_recv() {
                if event.id == show_id {
                    *show_window.lock() = true;
                } else if event.id == quit_id {
                    std::process::exit(0);
                }
            }

            if let Ok(_event) = hotkey_channel.try_recv() {
                let mut window_visible = show_window.lock();
                *window_visible = !*window_visible;
            }

            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    });

    Ok(())
}

fn main() -> Result<(), eframe::Error> {
    unsafe {
        std::env::set_var("WGPU_BACKEND", "gl");
    }

    let show_window = Arc::new(Mutex::new(false));

    #[cfg(not(target_os = "linux"))]
    {
        if let Err(e) = setup_tray_and_hotkey(show_window.clone()) {
            eprintln!("Failed to setup tray/hotkey: {}", e);
        }
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([600.0, 700.0])
            .with_always_on_top()
            .with_decorations(false)
            .with_transparent(true)
            .with_visible(false),
            renderer: eframe::Renderer::Glow,
            ..Default::default()
    };

    let show_window_clone = show_window.clone();
    eframe::run_native(
        "Clipper",
        options,
        Box::new(move |cc| Ok(Box::new(ClipperGui::new(cc, show_window_clone)))),
    )
}
