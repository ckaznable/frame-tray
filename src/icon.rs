use std::path::{Path, PathBuf};

/// Target edge length (px) used when rasterizing SVGs and choosing theme icon sizes.
pub const ICON_SIZE: u32 = 64;

#[derive(Clone)]
pub struct Icon {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
    pub premultiplied: bool,
}

/// Convert an SNI `IconPixmap` entry (ARGB32, network byte order) to RGBA.
pub fn from_argb32(width: i32, height: i32, data: &[u8]) -> Option<Icon> {
    let (w, h) = (usize::try_from(width).ok()?, usize::try_from(height).ok()?);
    if w == 0 || h == 0 || data.len() < w * h * 4 {
        return None;
    }
    let rgba = data[..w * h * 4]
        .chunks_exact(4)
        .flat_map(|p| [p[1], p[2], p[3], p[0]])
        .collect();
    Some(Icon { width: w, height: h, rgba, premultiplied: false })
}

/// Resolve a freedesktop icon name (or absolute path) to an image.
pub fn load_named(name: &str, theme_path: Option<&str>) -> Option<Icon> {
    if name.is_empty() {
        return None;
    }
    if name.starts_with('/') {
        return load_file(Path::new(name));
    }
    find_named(name, theme_path).and_then(|p| load_file(&p))
}

fn find_named(name: &str, theme_path: Option<&str>) -> Option<PathBuf> {
    // Apps may ship private icons via `IconThemePath`.
    if let Some(dir) = theme_path.filter(|d| !d.is_empty())
        && let Some(p) = search_dir(Path::new(dir), name, 4)
    {
        return Some(p);
    }

    let mut themes: Vec<String> = Vec::new();
    if let Ok(t) = std::env::var("FRAME_TRAY_ICON_THEME") {
        themes.push(t);
    }
    if let Some(t) = freedesktop_icons::default_theme_gtk() {
        themes.push(t);
    }
    themes.extend(["breeze", "Adwaita", "hicolor"].map(String::from));

    let size = ICON_SIZE as u16;
    themes
        .iter()
        .find_map(|t| freedesktop_icons::lookup(name).with_size(size).with_theme(t).with_cache().find())
        .or_else(|| {
            ["png", "svg", "xpm"]
                .iter()
                .map(|ext| PathBuf::from(format!("/usr/share/pixmaps/{name}.{ext}")))
                .find(|p| p.is_file())
        })
}

fn search_dir(dir: &Path, name: &str, depth: u32) -> Option<PathBuf> {
    for ext in ["png", "svg"] {
        let p = dir.join(format!("{name}.{ext}"));
        if p.is_file() {
            return Some(p);
        }
    }
    if depth == 0 {
        return None;
    }
    let mut subdirs: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    subdirs.sort();
    // Prefer larger size directories such as "64x64" or "scalable" over "16x16".
    subdirs.reverse();
    subdirs.iter().find_map(|d| search_dir(d, name, depth - 1))
}

fn load_file(path: &Path) -> Option<Icon> {
    let data = std::fs::read(path).ok()?;
    match path.extension().and_then(|e| e.to_str()) {
        Some("svg" | "svgz") => load_svg(&data),
        _ => {
            let img = image::load_from_memory(&data).ok()?.to_rgba8();
            Some(Icon {
                width: img.width() as usize,
                height: img.height() as usize,
                rgba: img.into_raw(),
                premultiplied: false,
            })
        }
    }
}

fn load_svg(data: &[u8]) -> Option<Icon> {
    let tree = resvg::usvg::Tree::from_data(data, &resvg::usvg::Options::default()).ok()?;
    let size = tree.size();
    let scale = ICON_SIZE as f32 / size.width().max(size.height());
    let mut pixmap = resvg::tiny_skia::Pixmap::new(ICON_SIZE, ICON_SIZE)?;
    resvg::render(&tree, resvg::tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
    Some(Icon {
        width: ICON_SIZE as usize,
        height: ICON_SIZE as usize,
        rgba: pixmap.take(),
        premultiplied: true,
    })
}
