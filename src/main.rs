// Hide the console window in release builds on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui::{
    self, pos2, vec2, Align, Align2, Color32, ColorImage, Key, Layout, Modifiers, Rect, RichText,
    TextureHandle, TextureId, TextureOptions, ViewportBuilder, ViewportCommand,
};
use serde::{Deserialize, Serialize};
use tiny_skia::{
    BlendMode, FillRule, LineCap, Paint, PathBuilder, Pixmap, PixmapPaint, Rect as SkRect, Stroke,
    Transform,
};

const CONFIG_FILE: &str = "config.json";

// --- CONFIG MANAGEMENT ---
// Field order matches the original DEFAULT_CONFIG so config.json keeps the same key order.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct Config {
    shape: String,
    length_x: i32,
    length_y: i32,
    link_lengths: bool,
    gap: i32,
    thickness: i32,
    opacity: i32,
    dot: bool,
    color: [u8; 4],
    outline: bool,
    outline_thickness: i32,
    outline_color: [u8; 4],
    glow: bool,
    // Legacy single 'length' key; kept only so it round-trips like the original.
    #[serde(skip_serializing_if = "Option::is_none")]
    length: Option<i32>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            shape: "Cross".into(),
            length_x: 10,
            length_y: 10,
            link_lengths: false,
            gap: 4,
            thickness: 2,
            opacity: 100,
            dot: false,
            color: [255, 0, 255, 255],
            outline: true,
            outline_thickness: 1,
            outline_color: [0, 0, 0, 255],
            glow: true,
            length: None,
        }
    }
}

fn load_config() -> Config {
    let mut base = serde_json::to_value(Config::default()).unwrap();
    if let Ok(text) = std::fs::read_to_string(CONFIG_FILE) {
        if let Ok(serde_json::Value::Object(data)) = serde_json::from_str::<serde_json::Value>(&text)
        {
            let map = base.as_object_mut().unwrap();
            for (k, v) in &data {
                map.insert(k.clone(), v.clone());
            }
            // Migration for configs with single 'length'
            if let Some(len) = data.get("length") {
                if !data.contains_key("length_x") {
                    map.insert("length_x".into(), len.clone());
                }
                if !data.contains_key("length_y") {
                    map.insert("length_y".into(), len.clone());
                }
            }
        }
    }
    serde_json::from_value(base).unwrap_or_default()
}

fn save_config(cfg: &Config) {
    let mut buf = Vec::new();
    let fmt = serde_json::ser::PrettyFormatter::with_indent(b"    ");
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, fmt);
    if cfg.serialize(&mut ser).is_ok() {
        let _ = std::fs::write(CONFIG_FILE, buf);
    }
}

// --- CROSSHAIR RENDERING (tiny-skia, mirrors the Qt layer pipeline) ---
const GLOW_SIZE: i32 = 1; // px of soft glow between the core and the outline
const GLOW_STRENGTH: f64 = 1.0; // raise (e.g. 1.5) for a brighter glow, lower for softer
const GLOW_WHITE_MIX: f64 = 0.6; // how white-hot the core center gets (0-1)

struct Geo {
    cx: f32,
    cy: f32,
    rx: f32,
    ry: f32,
    is_circle: bool,
    has_dot: bool,
    lines: Vec<[f32; 4]>,
    dpr: f32,
}

fn stroke(pm: &mut Pixmap, g: &Geo, width_px: f32, c: [u8; 4], aa: bool, mode: BlendMode) {
    let mut paint = Paint::default();
    paint.set_color_rgba8(c[0], c[1], c[2], c[3]);
    paint.anti_alias = aa;
    paint.blend_mode = mode;
    let ts = Transform::from_scale(g.dpr, g.dpr);

    if g.is_circle {
        if let Some(r) = SkRect::from_ltrb(g.cx - g.rx, g.cy - g.ry, g.cx + g.rx, g.cy + g.ry) {
            if let Some(path) = PathBuilder::from_oval(r) {
                let st = Stroke { width: width_px, ..Default::default() };
                pm.stroke_path(&path, &paint, &st, ts, None);
            }
        }
        return;
    }

    // Non-AA odd widths sit on the pixel center, like Qt's raster engine.
    let off = if !aa && (width_px as i32) % 2 == 1 { 0.5 / g.dpr } else { 0.0 };

    let st = Stroke { width: width_px, line_cap: LineCap::Square, ..Default::default() };
    for l in &g.lines {
        let mut pb = PathBuilder::new();
        pb.move_to(l[0] + off, l[1] + off);
        pb.line_to(l[2] + off, l[3] + off);
        if let Some(path) = pb.finish() {
            pm.stroke_path(&path, &paint, &st, ts, None);
        }
    }
    if g.has_dot {
        let r = width_px / 2.0;
        if let Some(rect) =
            SkRect::from_ltrb(g.cx + off - r, g.cy + off - r, g.cx + off + r, g.cy + off + r)
        {
            if let Some(path) = PathBuilder::from_oval(rect) {
                pm.fill_path(&path, &paint, FillRule::Winding, ts, None);
            }
        }
    }
}

fn render_crosshair(cfg: &Config, width: i32, height: i32, dpr: f32) -> Option<Pixmap> {
    let pw = ((width as f32 * dpr) as u32).max(1);
    let ph = ((height as f32 * dpr) as u32).max(1);
    let mut img = Pixmap::new(pw, ph)?;

    let cx = (width / 2) as f32;
    let cy = (height / 2) as f32;
    let (cxi, cyi) = (width / 2, height / 2);
    let shape = cfg.shape.as_str();
    let color = cfg.color;
    let outline_color = cfg.outline_color;
    let (len_x, len_y) = (cfg.length_x, cfg.length_y);
    let gap = cfg.gap;
    let thick = cfg.thickness;
    let outline = cfg.outline && cfg.outline_thickness > 0;
    let out_thick = cfg.outline_thickness;
    let glow = cfg.glow;

    let gw = if glow { GLOW_SIZE } else { 0 };
    let inner = thick + gw * 2;
    let is_circle = shape == "Circle";

    let arm_x = (len_x - gw).max(0);
    let arm_y = (len_y - gw).max(0);
    let rx = (gap + arm_x).max(1) as f32;
    let ry = (gap + arm_y).max(1) as f32;

    // Build line list for Cross / T-Shape / Dot
    let mut lines: Vec<[f32; 4]> = Vec::new();
    if !is_circle && shape != "Dot" {
        lines.push([(cxi - gap - arm_x) as f32, cyi as f32, (cxi - gap) as f32, cyi as f32]);
        lines.push([(cxi + gap) as f32, cyi as f32, (cxi + gap + arm_x) as f32, cyi as f32]);
        lines.push([cxi as f32, (cyi + gap) as f32, cxi as f32, (cyi + gap + arm_y) as f32]);
        if shape == "Cross" {
            lines.push([cxi as f32, (cyi - gap) as f32, cxi as f32, (cyi - gap - arm_y) as f32]);
        }
    }
    let has_dot = !is_circle && (shape == "Dot" || cfg.dot);

    let g = Geo { cx, cy, rx, ry, is_circle, has_dot, lines, dpr };
    let aa = is_circle;

    // Layer 1: outline (outermost)
    if outline {
        stroke(
            &mut img,
            &g,
            (inner + out_thick * 2) as f32,
            outline_color,
            aa,
            BlendMode::SourceOver,
        );
    }

    // Layer 2: soft neon glow
    if glow {
        for i in (1..=gw).rev() {
            let a = ((115.0 * GLOW_STRENGTH) as u32).min(255) as u8;
            let gc = [color[0], color[1], color[2], a];
            stroke(&mut img, &g, (thick + i * 2) as f32, gc, true, BlendMode::SourceOver);
        }
    }

    // Layer 3: core with a white-hot center
    let mut layer = Pixmap::new(pw, ph)?;
    stroke(&mut layer, &g, thick as f32, color, aa, BlendMode::SourceOver);

    if glow {
        let m = GLOW_WHITE_MIX;
        let mix = |c: u8| -> u8 { (c as f64 + (255.0 - c as f64) * m) as u8 };
        let tint = [mix(color[0]), mix(color[1]), mix(color[2])];
        for (frac, alpha) in [(1.0f32, 40.0f64), (0.65, 110.0), (0.35, 170.0)] {
            let a = ((alpha * GLOW_STRENGTH) as u32).min(255) as u8;
            let tc = [tint[0], tint[1], tint[2], a];
            stroke(&mut layer, &g, (thick as f32 * frac).max(1.0), tc, true, BlendMode::SourceAtop);
        }
    }

    img.draw_pixmap(
        0,
        0,
        layer.as_ref(),
        &PixmapPaint::default(),
        Transform::identity(),
        None,
    );

    // Overall opacity (premultiplied, so scale every channel)
    let o = (cfg.opacity as f32 / 100.0).clamp(0.0, 1.0);
    if o < 1.0 {
        for b in img.data_mut() {
            *b = (*b as f32 * o).round() as u8;
        }
    }
    Some(img)
}

// --- WINDOW ICON (rendered with the same crosshair renderer) ---
fn window_icon() -> egui::IconData {
    let size = 64u32;
    let cfg = Config {
        thickness: 4,
        gap: 7,
        length_x: 17,
        length_y: 17,
        outline_thickness: 2,
        ..Config::default()
    };
    let mut bg = Pixmap::new(size, size).unwrap();
    let mut paint = Paint::default();
    paint.set_color_rgba8(0x1e, 0x1e, 0x1e, 255);
    paint.anti_alias = true;
    if let Some(path) = PathBuilder::from_circle(32.0, 32.0, 32.0) {
        bg.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
    }
    if let Some(ch) = render_crosshair(&cfg, size as i32, size as i32, 1.0) {
        bg.draw_pixmap(0, 0, ch.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
    }
    let mut rgba = Vec::with_capacity((size * size * 4) as usize);
    for px in bg.pixels() {
        let c = px.demultiply();
        rgba.extend_from_slice(&[c.red(), c.green(), c.blue(), c.alpha()]);
    }
    egui::IconData { rgba, width: size, height: size }
}

// --- TEXTURE CACHE ---
#[derive(Default)]
struct Cached {
    tex: Option<TextureHandle>,
    key: Option<(Config, i32, i32, u32)>,
}

fn texture(
    cache: &mut Cached,
    ctx: &egui::Context,
    name: &str,
    cfg: &Config,
    w: i32,
    h: i32,
) -> Option<TextureId> {
    let ppp = ctx.pixels_per_point();
    let key = (cfg.clone(), w, h, ppp.to_bits());
    if cache.key.as_ref() != Some(&key) {
        if let Some(pm) = render_crosshair(cfg, w, h, ppp) {
            let img = ColorImage::from_rgba_premultiplied(
                [pm.width() as usize, pm.height() as usize],
                pm.data(),
            );
            match &mut cache.tex {
                Some(t) => t.set(img, TextureOptions::NEAREST),
                None => cache.tex = Some(ctx.load_texture(name, img, TextureOptions::NEAREST)),
            }
        }
        cache.key = Some(key);
    }
    cache.tex.as_ref().map(|t| t.id())
}

// --- NATIVE OVERLAY WINDOW (Win32 layered window, per-pixel alpha) ---
// Same mechanism Qt uses for WA_TranslucentBackground: WS_EX_LAYERED with
// UpdateLayeredWindow. Click-through via WS_EX_TRANSPARENT, as in the original.
mod overlay {
    use super::{render_crosshair, Config};
    use std::ffi::c_void;
    use std::ptr::null_mut;
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, SIZE, WPARAM};
    use windows_sys::Win32::Graphics::Gdi::{
        CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC,
        SelectObject, AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO, BI_RGB, BLENDFUNCTION,
        DIB_RGB_COLORS,
    };
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, GetSystemMetrics, RegisterClassW,
        ShowWindow, UpdateLayeredWindow, SM_CXSCREEN, SM_CYSCREEN, SW_HIDE, SW_SHOWNOACTIVATE,
        ULW_ALPHA, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
        WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
    };

    unsafe extern "system" fn wndproc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
        DefWindowProcW(h, m, w, l)
    }

    pub struct Overlay {
        hwnd: HWND,
        shown: bool,
        last: Option<(Config, u32)>,
    }

    impl Overlay {
        pub fn new() -> Self {
            unsafe {
                let class: Vec<u16> = "CrosshairOverlay\0".encode_utf16().collect();
                let hinst = GetModuleHandleW(std::ptr::null());
                let mut wc: WNDCLASSW = std::mem::zeroed();
                wc.lpfnWndProc = Some(wndproc);
                wc.hInstance = hinst;
                wc.lpszClassName = class.as_ptr();
                RegisterClassW(&wc);
                let hwnd = CreateWindowExW(
                    WS_EX_LAYERED
                        | WS_EX_TRANSPARENT
                        | WS_EX_TOPMOST
                        | WS_EX_TOOLWINDOW
                        | WS_EX_NOACTIVATE,
                    class.as_ptr(),
                    class.as_ptr(),
                    WS_POPUP,
                    0,
                    0,
                    1,
                    1,
                    null_mut(),
                    null_mut(),
                    hinst,
                    std::ptr::null(),
                );
                Self { hwnd, shown: false, last: None }
            }
        }

        /// Re-render and show/hide as needed. `ppp` is the editor's pixels-per-point.
        pub fn sync(&mut self, cfg: &Config, ppp: f32, visible: bool) {
            let key = (cfg.clone(), ppp.to_bits());
            if self.last.as_ref() != Some(&key) {
                // Same window-size formula as the original update_geometry()
                let max_len = cfg.length_x.max(cfg.length_y);
                let win =
                    ((max_len + cfg.gap + cfg.outline_thickness + 10) * 4 + 20).max(100);
                if let Some(pm) = render_crosshair(cfg, win, win, ppp) {
                    unsafe {
                        let sw = GetSystemMetrics(SM_CXSCREEN);
                        let sh = GetSystemMetrics(SM_CYSCREEN);
                        let x = (sw - pm.width() as i32).div_euclid(2);
                        let y = (sh - pm.height() as i32).div_euclid(2);
                        self.present(&pm, x, y);
                    }
                }
                self.last = Some(key);
            }
            if visible != self.shown {
                unsafe {
                    ShowWindow(self.hwnd, if visible { SW_SHOWNOACTIVATE } else { SW_HIDE });
                }
                self.shown = visible;
            }
        }

        unsafe fn present(&self, pm: &tiny_skia::Pixmap, x: i32, y: i32) {
            let w = pm.width() as i32;
            let h = pm.height() as i32;
            let screen = GetDC(null_mut());
            let mem = CreateCompatibleDC(screen);

            let mut bmi: BITMAPINFO = std::mem::zeroed();
            bmi.bmiHeader.biSize = std::mem::size_of::<windows_sys::Win32::Graphics::Gdi::BITMAPINFOHEADER>() as u32;
            bmi.bmiHeader.biWidth = w;
            bmi.bmiHeader.biHeight = -h; // top-down
            bmi.bmiHeader.biPlanes = 1;
            bmi.bmiHeader.biBitCount = 32;
            bmi.bmiHeader.biCompression = BI_RGB;

            let mut bits: *mut c_void = null_mut();
            let bmp = CreateDIBSection(screen, &bmi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
            if !bmp.is_null() && !bits.is_null() {
                let dst = std::slice::from_raw_parts_mut(bits as *mut u8, (w * h * 4) as usize);
                // tiny-skia: premultiplied RGBA. Windows wants premultiplied BGRA.
                for (d, s) in dst.chunks_exact_mut(4).zip(pm.data().chunks_exact(4)) {
                    d[0] = s[2];
                    d[1] = s[1];
                    d[2] = s[0];
                    d[3] = s[3];
                }
                let old = SelectObject(mem, bmp);
                let pt_dst = POINT { x, y };
                let size = SIZE { cx: w, cy: h };
                let pt_src = POINT { x: 0, y: 0 };
                let blend = BLENDFUNCTION {
                    BlendOp: AC_SRC_OVER as u8,
                    BlendFlags: 0,
                    SourceConstantAlpha: 255,
                    AlphaFormat: AC_SRC_ALPHA as u8,
                };
                UpdateLayeredWindow(
                    self.hwnd, screen, &pt_dst, &size, mem, &pt_src, 0, &blend, ULW_ALPHA,
                );
                SelectObject(mem, old);
            }
            if !bmp.is_null() {
                DeleteObject(bmp);
            }
            DeleteDC(mem);
            ReleaseDC(null_mut(), screen);
        }
    }

    impl Drop for Overlay {
        fn drop(&mut self) {
            unsafe {
                DestroyWindow(self.hwnd);
            }
        }
    }
}

use overlay::Overlay;

// --- APP ---
#[derive(Clone, Copy, PartialEq)]
enum Target {
    Main,
    Outline,
}

struct Picker {
    target: Target,
    color: Color32,
}

struct App {
    cfg: Config,
    overlay_visible: bool,
    picker: Option<Picker>,
    preview_cache: Cached,
    overlay: Overlay,
    fit_h: f32,
}

impl App {
    fn new(cfg: Config) -> Self {
        Self {
            cfg,
            overlay_visible: true,
            picker: None,
            preview_cache: Cached::default(),
            overlay: Overlay::new(),
            fit_h: 0.0,
        }
    }

    fn slider(ui: &mut egui::Ui, label: &str, v: &mut i32, min: i32, max: i32, unit: &str) -> bool {
        ui.label(format!("{label}: {v}{unit}"));
        ui.spacing_mut().slider_width = ui.available_width();
        ui.add(egui::Slider::new(v, min..=max).show_value(false)).changed()
    }

    fn editor_ui(&mut self, ui: &mut egui::Ui) {
        let mut dirty = false;

        // Preview Box
        ui.label("Live Preview:");
        ui.vertical_centered(|ui| {
            let (rect, _) = ui.allocate_exact_size(vec2(280.0, 110.0), egui::Sense::hover());
            let painter = ui.painter_at(rect);
            painter.rect_filled(rect, 6.0, Color32::from_rgb(0x1e, 0x1e, 0x1e));
            painter.rect_stroke(
                rect.shrink(1.0),
                6.0,
                egui::Stroke::new(2.0, Color32::from_rgb(0x33, 0x33, 0x33)),
                egui::StrokeKind::Inside,
            );
            if let Some(id) = texture(&mut self.preview_cache, ui.ctx(), "preview", &self.cfg, 280, 110)
            {
                painter.image(
                    id,
                    rect,
                    Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                    Color32::WHITE,
                );
            }
        });

        // Shape Selector
        ui.label("Crosshair Shape:");
        let before = self.cfg.shape.clone();
        egui::ComboBox::from_id_salt("shape")
            .width(ui.available_width())
            .selected_text(self.cfg.shape.clone())
            .show_ui(ui, |ui| {
                for s in ["Cross", "T-Shape", "Circle", "Dot"] {
                    ui.selectable_value(&mut self.cfg.shape, s.to_string(), s);
                }
            });
        if self.cfg.shape != before {
            dirty = true;
        }

        // Sliders & Link Checkbox
        if ui.checkbox(&mut self.cfg.link_lengths, "Link X & Y Length").changed() {
            if self.cfg.link_lengths && self.cfg.length_y != self.cfg.length_x {
                self.cfg.length_y = self.cfg.length_x;
            }
            dirty = true;
        }

        if Self::slider(ui, "Length X", &mut self.cfg.length_x, 1, 50, "px") {
            if self.cfg.link_lengths {
                self.cfg.length_y = self.cfg.length_x;
            }
            dirty = true;
        }
        if Self::slider(ui, "Length Y", &mut self.cfg.length_y, 1, 50, "px") {
            if self.cfg.link_lengths {
                self.cfg.length_x = self.cfg.length_y;
            }
            dirty = true;
        }
        dirty |= Self::slider(ui, "Center Gap", &mut self.cfg.gap, 0, 30, "px");
        dirty |= Self::slider(ui, "Thickness", &mut self.cfg.thickness, 1, 20, "px");
        dirty |= Self::slider(ui, "Opacity", &mut self.cfg.opacity, 10, 100, "%");

        // Effects Group
        ui.group(|ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new("Effects & Outline").strong());
            dirty |= ui.checkbox(&mut self.cfg.glow, "Enable Neon Glow").changed();
            dirty |= ui.checkbox(&mut self.cfg.outline, "Enable Outline").changed();
            dirty |= Self::slider(
                ui,
                "Outline Thickness",
                &mut self.cfg.outline_thickness,
                1,
                10,
                "px",
            );
        });

        // Colors Group
        ui.group(|ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new("Colors").strong());
            ui.columns(2, |cols| {
                if cols[0]
                    .add_sized([cols[0].available_width(), 24.0], egui::Button::new("Main Color"))
                    .clicked()
                {
                    let c = self.cfg.color;
                    self.picker = Some(Picker {
                        target: Target::Main,
                        color: Color32::from_rgb(c[0], c[1], c[2]),
                    });
                }
                if cols[1]
                    .add_sized([cols[1].available_width(), 24.0], egui::Button::new("Outline Color"))
                    .clicked()
                {
                    let c = self.cfg.outline_color;
                    self.picker = Some(Picker {
                        target: Target::Outline,
                        color: Color32::from_rgb(c[0], c[1], c[2]),
                    });
                }
            });
        });

        if dirty {
            save_config(&self.cfg);
        }
    }

    fn picker_ui(&mut self, ctx: &egui::Context) {
        let mut apply: Option<(Target, Color32)> = None;
        let mut close = false;
        if let Some(p) = &mut self.picker {
            let target = p.target;
            let title = match target {
                Target::Main => "Select Main Color",
                Target::Outline => "Select Outline Color",
            };
            egui::Window::new(title)
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, vec2(0.0, 0.0))
                .show(ctx, |ui| {
                    egui::color_picker::color_picker_color32(
                        ui,
                        &mut p.color,
                        egui::color_picker::Alpha::Opaque,
                    );
                    ui.horizontal(|ui| {
                        if ui.button("OK").clicked() {
                            apply = Some((target, p.color));
                            close = true;
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                });
        }
        if let Some((target, c)) = apply {
            let rgba = [c.r(), c.g(), c.b(), 255];
            match target {
                Target::Main => self.cfg.color = rgba,
                Target::Outline => self.cfg.outline_color = rgba,
            }
            save_config(&self.cfg);
        }
        if close {
            self.picker = None;
        }
    }
}

impl eframe::App for App {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // F8 shortcut (active while the editor window has focus, like the QShortcut)
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::F8)) {
            self.overlay_visible = !self.overlay_visible;
        }

        let content_bottom = egui::CentralPanel::default()
            .show(ctx, |ui| {
                ui.with_layout(Layout::top_down(Align::Min), |ui| self.editor_ui(ui));
                ui.min_rect().max.y
            })
            .inner;

        // Shrink the fixed-size window to exactly fit its content (panel margin below).
        let h = (content_bottom + 8.0).ceil();
        if (h - self.fit_h).abs() > 0.5 {
            self.fit_h = h;
            ctx.send_viewport_cmd(ViewportCommand::InnerSize(vec2(320.0, h)));
        }
        self.picker_ui(ctx);
        self.overlay
            .sync(&self.cfg, ctx.pixels_per_point(), self.overlay_visible);
    }
}

fn main() -> eframe::Result {
    let cfg = load_config();

    let options = eframe::NativeOptions {
        viewport: ViewportBuilder::default()
            .with_title("Crosshair Customizer")
            .with_inner_size([320.0, 590.0])
            .with_resizable(false)
            .with_maximize_button(false)
            .with_always_on_top()
            .with_icon(window_icon()),
        ..Default::default()
    };

    eframe::run_native(
        "Crosshair Customizer",
        options,
        Box::new(|_cc| Ok(Box::new(App::new(cfg)))),
    )
}
