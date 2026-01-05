// clipper/src/lib.rs
use crux_core::macros::Effect;
use crux_core::{render::Render, App, Command};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ClipEntry {
    pub id: i64,
    pub hash: String,
    pub content: String,
    pub time_ago: String,
    pub entry_type: String,
    pub size_str: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Model {
    pub clips: Vec<ClipEntry>,
    pub search_query: String,
    pub selected_index: usize,
    pub theme: String,
    pub full_search_mode: bool,
    pub loading: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum Event {
    Init,
    RefreshClips,
    ClipsLoaded(Vec<ClipEntry>),
    UpdateSearch(String),
    SelectIndex(usize),
    CopyClip(usize),
    Copied,
    KeyPress(Key),
    ToggleSearchMode,
    SearchWithQuery(String),
    // Shell request events
    LoadClips,
    CopyToClipboard(String),
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum Key {
    Up,
    Down,
    Enter,
    Escape,
}

#[derive(Effect)]
pub struct Capabilities {
    render: Render<Event>,
}

#[derive(Default)]
pub struct ClipperApp;

impl App for ClipperApp {
    type Event = Event;
    type Model = Model;
    type ViewModel = Model;
    type Capabilities = Capabilities;
    type Effect = Effect;

    fn update(
        &self,
        event: Self::Event,
        model: &mut Self::Model,
        caps: &Self::Capabilities,
    ) -> Command<Self::Effect, Self::Event> {
        match event {
            Event::Init | Event::RefreshClips => {
                caps.render.render();
                Command::event(Event::LoadClips)
            }
            Event::LoadClips => {
                // This will be handled by the shell
                Command::done()
            }
            Event::ClipsLoaded(clips) => {
                model.clips = clips;
                caps.render.render();
                Command::done()
            }
            Event::UpdateSearch(query) => {
                model.search_query = query;
                model.selected_index = 0;
                caps.render.render();
                Command::done()
            }
            Event::SelectIndex(index) => {
                model.selected_index = index;
                caps.render.render();
                Command::done()
            }
            Event::CopyClip(index) => {
                if let Some(clip) = self.filtered_clips(model).get(index) {
                    let content = clip.content.clone();
                    Command::event(Event::CopyToClipboard(content))
                } else {
                    Command::done()
                }
            }
            Event::CopyToClipboard(_) => {
                // This will be handled by the shell
                Command::done()
            }
            Event::Copied => {
                caps.render.render();
                Command::done()
            }
            Event::ToggleSearchMode => {
                model.full_search_mode = !model.full_search_mode;
                caps.render.render();
                // Trigger a refresh with the new mode
                Command::event(Event::RefreshClips)
            }
            Event::SearchWithQuery(query) => {
                model.search_query = query;
                model.selected_index = 0;
                caps.render.render();
                Command::done()
            }
            Event::KeyPress(key) => {
                match key {
                    Key::Up => {
                        if model.selected_index > 0 {
                            model.selected_index -= 1;
                            caps.render.render();
                        }
                        Command::done()
                    }
                    Key::Down => {
                        let max = self.filtered_clips(model).len().saturating_sub(1);
                        if model.selected_index < max {
                            model.selected_index += 1;
                            caps.render.render();
                        }
                        Command::done()
                    }
                    Key::Enter => self.update(Event::CopyClip(model.selected_index), model, caps),
                    Key::Escape => {
                        // Handle in shell (close window)
                        Command::done()
                    }
                }
            }
        }
    }

    fn view(&self, model: &Self::Model) -> Self::ViewModel {
        model.clone()
    }
}

impl ClipperApp {
    fn filtered_clips(&self, model: &Model) -> Vec<ClipEntry> {
        if model.search_query.is_empty() {
            model.clips.clone()
        } else {
            let query = model.search_query.to_lowercase();
            model
                .clips
                .iter()
                .filter(|clip| clip.content.to_lowercase().contains(&query))
                .cloned()
                .collect()
        }
    }
}
