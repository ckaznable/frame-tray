mod icon;
mod menu;
mod sni;

use std::sync::mpsc::Receiver;

use eframe::egui::{self, Color32, TextureHandle, TextureOptions, Vec2};
use tokio::sync::mpsc::UnboundedSender;

use menu::{MenuNode, Toggle};
use sni::{Command, Status, TrayItem, UiEvent};

const BUTTON_ICON: f32 = 40.0;

fn main() -> eframe::Result {
    let on_top = std::env::args().any(|a| a == "--on-top");
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Frame Tray")
        .with_app_id("frame-tray")
        .with_inner_size([360.0, 420.0])
        .with_min_inner_size([160.0, 120.0]);
    if on_top {
        viewport = viewport.with_always_on_top();
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    eframe::run_native("Frame Tray", options, Box::new(|cc| Ok(Box::new(App::new(&cc.egui_ctx)))))
}

struct Entry {
    item: TrayItem,
    texture: Option<TextureHandle>,
}

struct App {
    rx: Receiver<UiEvent>,
    cmd: UnboundedSender<Command>,
    entries: Vec<Entry>,
    status: String,
    /// Open menu: item key plus the chain of submenu ids we've descended into.
    menu_view: Option<(String, Vec<i32>)>,
}

impl App {
    fn new(ctx: &egui::Context) -> Self {
        add_system_fallback_font(ctx);
        let (tx, rx) = std::sync::mpsc::channel();
        let repaint_ctx = ctx.clone();
        let cmd = sni::spawn(tx, move || repaint_ctx.request_repaint());
        Self { rx, cmd, entries: Vec::new(), status: "Connecting to D-Bus…".into(), menu_view: None }
    }

    fn drain_events(&mut self, ctx: &egui::Context) {
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                UiEvent::Upsert(item) => {
                    let texture = item.icon.as_ref().map(|ic| {
                        let img = if ic.premultiplied {
                            egui::ColorImage::from_rgba_premultiplied([ic.width, ic.height], &ic.rgba)
                        } else {
                            egui::ColorImage::from_rgba_unmultiplied([ic.width, ic.height], &ic.rgba)
                        };
                        ctx.load_texture(&item.key, img, TextureOptions::LINEAR)
                    });
                    match self.entries.iter_mut().find(|e| e.item.key == item.key) {
                        Some(e) => *e = Entry { item, texture },
                        None => self.entries.push(Entry { item, texture }),
                    }
                }
                UiEvent::Remove(key) => {
                    self.entries.retain(|e| e.item.key != key);
                    if self.menu_view.as_ref().is_some_and(|(k, _)| *k == key) {
                        self.menu_view = None;
                    }
                }
                UiEvent::Status(s) => self.status = s,
            }
        }
    }

    fn send(&self, cmd: Command) {
        let _ = self.cmd.send(cmd);
    }

    fn open_menu(&mut self, key: &str) {
        self.send(Command::MenuOpened { key: key.to_owned() });
        self.menu_view = Some((key.to_owned(), Vec::new()));
    }

    fn tray_view(&mut self, ui: &mut egui::Ui) {
        if self.entries.is_empty() {
            ui.centered_and_justified(|ui| {
                ui.label(egui::RichText::new(format!("No tray items yet\n\n{}", self.status)).weak());
            });
            return;
        }

        // Screen position for Activate/ContextMenu; unknown on Wayland, so best effort.
        let origin = ui.ctx().input(|i| i.viewport().outer_rect.map(|r| r.min)).unwrap_or_default();
        let mut actions = Vec::new();

        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = Vec2::splat(8.0);
                for e in &self.entries {
                    let resp = item_button(ui, e);
                    let p = resp.interact_pointer_pos().unwrap_or(resp.rect.center()) + origin.to_vec2();
                    let (x, y) = (p.x as i32, p.y as i32);
                    let key = e.item.key.clone();
                    let has_menu = e.item.menu.as_ref().is_some_and(|m| !m.children.is_empty());

                    if resp.clicked() {
                        if e.item.item_is_menu && has_menu {
                            actions.push(Action::OpenMenu(key.clone()));
                        } else {
                            actions.push(Action::Cmd(Command::Activate { key: key.clone(), x, y }));
                        }
                    }
                    if resp.secondary_clicked() {
                        if has_menu {
                            actions.push(Action::OpenMenu(key.clone()));
                        } else {
                            actions.push(Action::Cmd(Command::ContextMenu { key: key.clone(), x, y }));
                        }
                    }
                    if resp.middle_clicked() {
                        actions.push(Action::Cmd(Command::SecondaryActivate { key: key.clone(), x, y }));
                    }
                    if resp.hovered() {
                        let delta = ui.input(wheel_delta);
                        if delta.y != 0.0 {
                            actions.push(Action::Cmd(Command::Scroll { key: key.clone(), delta: delta.y as i32, horizontal: false }));
                        } else if delta.x != 0.0 {
                            actions.push(Action::Cmd(Command::Scroll { key, delta: delta.x as i32, horizontal: true }));
                        }
                    }
                }
            });
        });

        for a in actions {
            match a {
                Action::OpenMenu(key) => self.open_menu(&key),
                Action::Cmd(c) => self.send(c),
            }
        }
    }

    fn menu_page(&mut self, ui: &mut egui::Ui) {
        let Some((key, path)) = self.menu_view.clone() else { return };
        let Some(entry) = self.entries.iter().find(|e| e.item.key == key) else {
            self.menu_view = None;
            return;
        };
        let Some(root) = entry.item.menu.as_ref() else {
            self.menu_view = None;
            return;
        };

        // Walk down to the current submenu; if the layout changed underneath us, stop where it breaks.
        let mut node = root;
        let mut valid_depth = 0;
        let mut crumbs = vec![entry.item.title.clone()];
        for id in &path {
            match node.children.iter().find(|c| c.id == *id) {
                Some(child) => {
                    node = child;
                    valid_depth += 1;
                    crumbs.push(child.label.clone());
                }
                None => break,
            }
        }

        let mut nav: Option<Nav> = None;
        ui.horizontal(|ui| {
            if ui.button("⏴ Back").clicked() {
                nav = Some(Nav::Up);
            }
            ui.label(egui::RichText::new(crumbs.join(" › ")).strong());
        });
        ui.separator();

        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.with_layout(egui::Layout::top_down_justified(egui::Align::Min), |ui| {
                ui.spacing_mut().button_padding = Vec2::new(10.0, 8.0);
                for child in node.children.iter().filter(|c| c.visible) {
                    if let Some(n) = menu_entry(ui, child) {
                        nav = Some(n);
                    }
                }
            });
        });

        let depth = valid_depth.min(path.len());
        match nav {
            Some(Nav::Up) if depth == 0 => self.menu_view = None,
            Some(Nav::Up) => self.menu_view = Some((key, path[..depth - 1].to_vec())),
            Some(Nav::Into(id)) => {
                let mut p = path[..depth].to_vec();
                p.push(id);
                self.menu_view = Some((key, p));
            }
            Some(Nav::Clicked(id)) => {
                self.send(Command::MenuClicked { key, id });
                self.menu_view = None;
            }
            None => {}
        }
    }
}

/// egui's bundled fonts have no CJK glyphs, and tray menus are often localized.
/// Ask fontconfig for a font covering the user's language and append it as a fallback.
fn add_system_fallback_font(ctx: &egui::Context) {
    let lang = std::env::var("LANG")
        .ok()
        .and_then(|l| l.split('.').next().map(|l| l.replace('_', "-").to_lowercase()))
        .filter(|l| !l.is_empty() && l != "c" && l != "posix");
    let queries = lang.into_iter().chain(["zh-tw".to_owned()]);

    let found = queries.into_iter().find_map(|lang| {
        let out = std::process::Command::new("fc-match")
            .args(["-f", "%{file}|%{index}", &format!("sans-serif:lang={lang}")])
            .output()
            .ok()?;
        let text = String::from_utf8(out.stdout).ok()?;
        let (file, index) = text.split_once('|')?;
        let data = std::fs::read(file).ok()?;
        Some((data, index.trim().parse().unwrap_or(0)))
    });
    let Some((data, index)) = found else { return };

    let mut fonts = egui::FontDefinitions::default();
    let font = egui::FontData { index, ..egui::FontData::from_owned(data) };
    fonts.font_data.insert("system-fallback".into(), std::sync::Arc::new(font));
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts.families.entry(family).or_default().push("system-fallback".into());
    }
    ctx.set_fonts(fonts);
}

enum Action {
    OpenMenu(String),
    Cmd(Command),
}

enum Nav {
    Up,
    Into(i32),
    Clicked(i32),
}

fn item_button(ui: &mut egui::Ui, e: &Entry) -> egui::Response {
    let size = Vec2::splat(BUTTON_ICON);
    let tint = if e.item.status == Status::Passive { Color32::from_white_alpha(110) } else { Color32::WHITE };
    let mut button = match &e.texture {
        Some(tex) => egui::Button::image(egui::Image::new(tex).fit_to_exact_size(size).tint(tint)),
        None => {
            let initial: String = e.item.title.chars().take(2).collect();
            egui::Button::new(egui::RichText::new(initial).size(18.0)).min_size(size + Vec2::splat(8.0))
        }
    };
    if e.item.status == Status::NeedsAttention {
        button = button.stroke(egui::Stroke::new(2.0, Color32::from_rgb(230, 120, 30)));
    }
    ui.add(button).on_hover_text(if e.item.tooltip.is_empty() { &e.item.title } else { &e.item.tooltip })
}

/// Wheel movement this frame in Qt-style angle units (120 per notch; positive = up/left).
fn wheel_delta(i: &egui::InputState) -> Vec2 {
    i.events.iter().fold(Vec2::ZERO, |acc, ev| match ev {
        egui::Event::MouseWheel { unit, delta, .. } => {
            let scale = match unit {
                egui::MouseWheelUnit::Point => 120.0 / 50.0,
                egui::MouseWheelUnit::Line => 120.0,
                egui::MouseWheelUnit::Page => 360.0,
            };
            acc + *delta * scale
        }
        _ => acc,
    })
}

fn menu_entry(ui: &mut egui::Ui, node: &MenuNode) -> Option<Nav> {
    if node.separator {
        ui.separator();
        return None;
    }
    let has_children = !node.children.is_empty();
    let label = match (&node.toggle, has_children) {
        (Toggle::Checkmark(on), _) => format!("{} {}", if *on { "☑" } else { "☐" }, node.label),
        (Toggle::Radio(on), _) => format!("{} {}", if *on { "◉" } else { "○" }, node.label),
        (Toggle::None, true) => format!("{}  ⏵", node.label),
        (Toggle::None, false) => node.label.clone(),
    };
    let resp = ui.add_enabled(node.enabled, egui::Button::new(label));
    if !resp.clicked() {
        return None;
    }
    Some(if has_children { Nav::Into(node.id) } else { Nav::Clicked(node.id) })
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.drain_events(ui.ctx());
        egui::CentralPanel::default().show(ui, |ui| {
            if self.menu_view.is_some() {
                self.menu_page(ui);
            } else {
                self.tray_view(ui);
            }
        });
    }
}
