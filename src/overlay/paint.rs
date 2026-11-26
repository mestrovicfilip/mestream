//! Drawing the overlay with Direct2D, straight onto the swap chain's back
//! buffer after the video has been drawn there.

use super::geometry::Rect;
use super::menu::{self, Item, Kind, Layout, TABS};
use super::prompt::{self, Prompt, Style};
use super::{Armed, QrCode, STATE};
use crate::settings::{self, Settings};
use crate::text::text;
use anyhow::{Result, anyhow};
use std::mem::ManuallyDrop;
use std::time::Instant;
use windows::Win32::Graphics::Direct2D::Common::{
    D2D_RECT_F, D2D1_ALPHA_MODE_IGNORE, D2D1_COLOR_F, D2D1_PIXEL_FORMAT,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1_BITMAP_OPTIONS_CANNOT_DRAW, D2D1_BITMAP_OPTIONS_TARGET, D2D1_BITMAP_PROPERTIES1,
    D2D1_DEVICE_CONTEXT_OPTIONS_NONE, D2D1_DRAW_TEXT_OPTIONS_CLIP, D2D1_FACTORY_TYPE_SINGLE_THREADED,
    D2D1_ROUNDED_RECT, D2D1CreateFactory, ID2D1DeviceContext, ID2D1Factory1, ID2D1SolidColorBrush,
};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::Graphics::DirectWrite::{
    DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT,
    DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_MEASURING_MODE_NATURAL,
    DWRITE_PARAGRAPH_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT, DWRITE_TEXT_ALIGNMENT_CENTER,
    DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_TEXT_ALIGNMENT_TRAILING, DWRITE_WORD_WRAPPING_NO_WRAP,
    DWriteCreateFactory, IDWriteFactory, IDWriteTextFormat,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Dxgi::{IDXGIDevice, IDXGISurface};
use windows::core::{HSTRING, Interface, w};

const MENU_SLIDE_SECONDS: f32 = 0.2;
const LOG_LINES_SHOWN: usize = 7;
const ACCENT: (f32, f32, f32) = (0.30, 0.82, 0.34);
const AMBER: (f32, f32, f32) = (1.0, 0.74, 0.30);
const RED: (f32, f32, f32) = (1.0, 0.45, 0.45);

/// What the renderer knows about this frame.
pub struct Scene {
    /// No video underneath: start from black.
    pub clear: bool,
    /// Opacity of the start-up screen: 1 while connecting, fading to 0 over
    /// the first moments of video.
    pub loader: f32,
}

/// The part of the shared state one frame needs, copied out so the lock is
/// not held while drawing.
struct Frame {
    menu_open: bool,
    toggled_at: Option<Instant>,
    tab: usize,
    /// The focused control, when the focus ring should be visible.
    focus: Option<usize>,
    armed: Option<Armed>,
    prompt: Option<Prompt>,
    qr: Option<QrCode>,
    copied: bool,
    log: Vec<(Instant, String)>,
    stats: String,
    console_name: String,
}

impl Frame {
    fn capture() -> Self {
        let state = STATE.lock().unwrap();
        Self {
            menu_open: state.menu_open,
            toggled_at: state.toggled_at,
            tab: state.tab,
            focus: state.focus_shown.then_some(state.focus),
            armed: state.armed.filter(|(what, _)| state.is_armed(*what)).map(|(what, _)| what),
            prompt: state.prompt.clone(),
            qr: state.prompt_qr.clone(),
            copied: state.copied_at.is_some_and(|at| at.elapsed().as_secs_f32() < 2.5),
            log: state.log.iter().rev().take(LOG_LINES_SHOWN + 1).cloned().collect(),
            stats: state.stats.clone(),
            console_name: state.console_name.clone(),
        }
    }
}

fn ease_out(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

/// How far the menu is slid in, 0 (hidden) to 1 (fully open).
fn menu_openness(open: bool, toggled_at: Option<Instant>) -> f32 {
    let progress = toggled_at
        .map(|t| t.elapsed().as_secs_f32() / MENU_SLIDE_SECONDS)
        .unwrap_or(1.0);
    if open { ease_out(progress) } else { 1.0 - ease_out(progress) }
}

/// Letter-spaced capitals for the wordmark.
fn spaced(text: &str) -> String {
    let mut out = String::new();
    for (i, c) in text.to_uppercase().chars().enumerate() {
        if i > 0 {
            out.push('\u{2009}');
        }
        out.push(c);
    }
    out
}

fn rgba(r: f32, g: f32, b: f32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r, g, b, a }
}

fn white(a: f32) -> D2D1_COLOR_F {
    rgba(1.0, 1.0, 1.0, a)
}

fn tint(color: (f32, f32, f32), a: f32) -> D2D1_COLOR_F {
    rgba(color.0, color.1, color.2, a)
}

fn d2d(rect: &Rect) -> D2D_RECT_F {
    D2D_RECT_F { left: rect.left, top: rect.top, right: rect.right, bottom: rect.bottom }
}

struct Fonts {
    unit: f32,
    title: IDWriteTextFormat,
    heading: IDWriteTextFormat,
    label: IDWriteTextFormat,
    value: IDWriteTextFormat,
    small: IDWriteTextFormat,
    about: IDWriteTextFormat,
    small_centred: IDWriteTextFormat,
    button: IDWriteTextFormat,
    tab: IDWriteTextFormat,
    glyph: IDWriteTextFormat,
    mono: IDWriteTextFormat,
    stats: IDWriteTextFormat,
    code: IDWriteTextFormat,
    message: IDWriteTextFormat,
    wordmark: IDWriteTextFormat,
}

pub struct Painter {
    context: ID2D1DeviceContext,
    brush: ID2D1SolidColorBrush,
    writer: IDWriteFactory,
    fonts: Option<Fonts>,
    started: Instant,
}

impl Painter {
    pub unsafe fn new(device: &ID3D11Device, back_buffer: &ID3D11Texture2D) -> Result<Self> {
        unsafe {
            let factory: ID2D1Factory1 = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let d2d_device = factory.CreateDevice(&device.cast::<IDXGIDevice>()?)?;
            let context = d2d_device.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?;
            let properties = D2D1_BITMAP_PROPERTIES1 {
                pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_IGNORE },
                dpiX: 96.0,
                dpiY: 96.0,
                bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
                colorContext: ManuallyDrop::new(None),
            };
            let target = context.CreateBitmapFromDxgiSurface(&back_buffer.cast::<IDXGISurface>()?, Some(&properties))?;
            context.SetTarget(&target);
            let writer: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            let brush = context.CreateSolidColorBrush(&white(1.0), None)?;
            Ok(Self { context, brush, writer, fonts: None, started: Instant::now() })
        }
    }

    unsafe fn font(
        &self,
        family: &str,
        weight: DWRITE_FONT_WEIGHT,
        size: f32,
        alignment: DWRITE_TEXT_ALIGNMENT,
        wrap: bool,
    ) -> Result<IDWriteTextFormat> {
        unsafe {
            let format = self.writer.CreateTextFormat(
                &HSTRING::from(family),
                None,
                weight,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                size,
                w!("en-us"),
            )?;
            format.SetTextAlignment(alignment)?;
            format.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            if !wrap {
                format.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
            }
            Ok(format)
        }
    }

    unsafe fn ensure_fonts(&mut self, u: f32) -> Result<()> {
        unsafe {
            if self.fonts.as_ref().is_some_and(|f| (f.unit - u).abs() < 0.01) {
                return Ok(());
            }
            let (ui, mono) = ("Segoe UI", "Cascadia Mono");
            let (regular, bold) = (DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_WEIGHT_SEMI_BOLD);
            let (lead, centre, trail) =
                (DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_TEXT_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT_TRAILING);
            self.fonts = Some(Fonts {
                unit: u,
                title: self.font(ui, bold, 28.0 * u, lead, false)?,
                heading: self.font(ui, bold, 26.0 * u, centre, false)?,
                label: self.font(ui, regular, 18.0 * u, lead, false)?,
                value: self.font(ui, bold, 18.0 * u, trail, false)?,
                small: self.font(ui, regular, 14.0 * u, lead, false)?,
                about: self.font(ui, regular, 13.0 * u, lead, false)?,
                small_centred: self.font(ui, regular, 15.0 * u, centre, false)?,
                button: self.font(ui, bold, 18.0 * u, centre, false)?,
                tab: self.font(ui, bold, 15.0 * u, centre, false)?,
                glyph: self.font(ui, regular, 26.0 * u, centre, false)?,
                mono: self.font(mono, regular, 13.0 * u, centre, false)?,
                stats: self.font(mono, regular, 13.0 * u, lead, false)?,
                code: self.font(mono, bold, 44.0 * u, centre, false)?,
                message: self.font(ui, regular, prompt::BODY_FONT * u, centre, true)?,
                wordmark: self.font(ui, bold, 13.0 * u, centre, false)?,
            });
            Ok(())
        }
    }

    unsafe fn fill(&self, rect: &Rect, radius: f32, color: D2D1_COLOR_F) {
        unsafe {
            self.brush.SetColor(&color);
            if radius <= 0.0 {
                self.context.FillRectangle(&d2d(rect), &self.brush);
            } else {
                let rounded = D2D1_ROUNDED_RECT { rect: d2d(rect), radiusX: radius, radiusY: radius };
                self.context.FillRoundedRectangle(&rounded, &self.brush);
            }
        }
    }

    unsafe fn outline(&self, rect: &Rect, radius: f32, color: D2D1_COLOR_F, width: f32) {
        unsafe {
            self.brush.SetColor(&color);
            let rounded = D2D1_ROUNDED_RECT { rect: d2d(rect), radiusX: radius, radiusY: radius };
            self.context.DrawRoundedRectangle(&rounded, &self.brush, width, None);
        }
    }

    unsafe fn dot(&self, x: f32, y: f32, radius: f32, color: D2D1_COLOR_F) {
        unsafe {
            let rect = Rect { left: x - radius, top: y - radius, right: x + radius, bottom: y + radius };
            self.fill(&rect, radius, color);
        }
    }

    unsafe fn text(&self, text: &str, format: &IDWriteTextFormat, rect: &Rect, color: D2D1_COLOR_F) {
        unsafe {
            self.brush.SetColor(&color);
            let wide: Vec<u16> = text.encode_utf16().collect();
            self.context.DrawText(
                &wide,
                format,
                &d2d(rect),
                &self.brush,
                D2D1_DRAW_TEXT_OPTIONS_CLIP,
                DWRITE_MEASURING_MODE_NATURAL,
            );
        }
    }

    /// A rounded button in one of the three styles, optionally lit up.
    unsafe fn button(&self, rect: &Rect, label: &str, style: Style, lit: bool, u: f32, alpha: f32) {
        unsafe {
            let fonts = self.fonts.as_ref().unwrap();
            let radius = 14.0 * u;
            let colour = match style {
                Style::Plain => None,
                Style::Warning => Some(AMBER),
                Style::Danger => Some(RED),
            };
            match colour {
                None => {
                    self.fill(rect, radius, white(if lit { 0.18 } else { 0.08 } * alpha));
                    self.outline(rect, radius, white(0.11 * alpha), 1.0);
                    self.text(label, &fonts.button, rect, white(0.92 * alpha));
                }
                Some(colour) => {
                    self.fill(rect, radius, tint(colour, if lit { 0.34 } else { 0.14 } * alpha));
                    self.outline(rect, radius, tint(colour, 0.5 * alpha), 1.0);
                    self.text(label, &fonts.button, rect, tint(colour, alpha));
                }
            }
        }
    }

    /// The ring around the control the controller or keyboard is on.
    unsafe fn focus_ring(&self, rect: &Rect, u: f32, alpha: f32) {
        unsafe {
            self.outline(&rect.inset(-3.0 * u), 16.0 * u, tint(ACCENT, 0.95 * alpha), 2.0 * u);
        }
    }

    /// A ring of dots with a bright head chasing a fading tail.
    unsafe fn spinner(&self, cx: f32, cy: f32, radius: f32, dot: f32, alpha: f32) {
        unsafe {
            const DOTS: usize = 12;
            let turn = self.started.elapsed().as_secs_f32() * 0.9;
            for i in 0..DOTS {
                let position = i as f32 / DOTS as f32;
                let angle = position * std::f32::consts::TAU - std::f32::consts::FRAC_PI_2;
                let behind_head = (turn - position).rem_euclid(1.0);
                let glow = (1.0 - behind_head).powi(3);
                let color = rgba(
                    1.0 + (ACCENT.0 - 1.0) * glow,
                    1.0 + (ACCENT.1 - 1.0) * glow,
                    1.0 + (ACCENT.2 - 1.0) * glow,
                    (0.10 + 0.90 * glow) * alpha,
                );
                self.dot(cx + angle.cos() * radius, cy + angle.sin() * radius, dot * (1.0 + 0.45 * glow), color);
            }
        }
    }

    /// Draws whatever this frame calls for. Costs next to nothing when there
    /// is nothing to draw.
    pub unsafe fn paint(&mut self, width: f32, height: f32, scene: &Scene) -> Result<()> {
        unsafe {
            let frame = Frame::capture();
            let settings = settings::get();
            let openness = menu_openness(frame.menu_open, frame.toggled_at);
            let show_stats = settings.show_stats && !scene.clear && !frame.stats.is_empty();
            if !scene.clear && scene.loader <= 0.0 && openness <= 0.001 && !show_stats {
                return Ok(());
            }

            let layout = menu::layout(width, height, frame.tab);
            let u = layout.unit;
            self.ensure_fonts(u)?;
            self.context.BeginDraw();
            if scene.clear {
                self.context.Clear(Some(&rgba(0.0, 0.0, 0.0, 1.0)));
            }
            if show_stats {
                self.paint_stats(&frame.stats, u);
            }
            if scene.loader > 0.0 {
                if !scene.clear {
                    // Fading out over the first video frames.
                    let all = Rect { left: 0.0, top: 0.0, right: width, bottom: height };
                    self.fill(&all, 0.0, rgba(0.0, 0.0, 0.0, scene.loader));
                }
                match &frame.prompt {
                    Some(prompt) => self.paint_prompt(width, height, prompt, &frame, scene.loader),
                    None => self.paint_loader(width, height, u, scene.loader, &frame.log),
                }
            }
            if openness > 0.001 {
                self.paint_menu(&layout, openness, &settings, &frame);
            }
            self.context
                .EndDraw(None, None)
                .map_err(|e| anyhow!("drawing the overlay failed: {e}"))
        }
    }

    unsafe fn paint_stats(&self, line: &str, u: f32) {
        unsafe {
            let fonts = self.fonts.as_ref().unwrap();
            let width = (line.chars().count() as f32 * 8.0 + 24.0) * u;
            let plate = Rect { left: 12.0 * u, top: 12.0 * u, right: 12.0 * u + width, bottom: 40.0 * u };
            self.fill(&plate, 8.0 * u, rgba(0.0, 0.0, 0.0, 0.55));
            let inner = Rect { left: plate.left + 12.0 * u, ..plate };
            self.text(line, &fonts.stats, &inner, white(0.85));
        }
    }

    unsafe fn paint_loader(&self, width: f32, height: f32, u: f32, alpha: f32, log: &[(Instant, String)]) {
        unsafe {
            let fonts = self.fonts.as_ref().unwrap();
            let (cx, cy) = (width / 2.0, height * 0.40);
            self.spinner(cx, cy, 24.0 * u, 2.6 * u, alpha);

            let wordmark = Rect { left: 0.0, top: cy + 46.0 * u, right: width, bottom: cy + 70.0 * u };
            self.text(&spaced("Mestream"), &fonts.wordmark, &wordmark, white(0.55 * alpha));

            // Newest line at the bottom, older ones rising and fading out.
            let line_height = 22.0 * u;
            let newest_age = log.first().map(|(at, _)| at.elapsed().as_secs_f32()).unwrap_or(1.0);
            let settle = ease_out(newest_age / 0.28);
            let bottom = cy + 96.0 * u + LOG_LINES_SHOWN as f32 * line_height;
            for (index, (_, line)) in log.iter().enumerate() {
                // While a new line settles in, everything is still one slot lower.
                let slot = index as f32 + settle - 1.0;
                let top = bottom - (slot + 1.0) * line_height;
                let age_fade = (1.0 - slot.max(0.0) / LOG_LINES_SHOWN as f32).clamp(0.0, 1.0).powf(1.6);
                let arrive = if index == 0 { settle } else { 1.0 };
                let rect = Rect { left: width * 0.1, top, right: width * 0.9, bottom: top + line_height };
                self.text(line, &fonts.mono, &rect, white(0.42 * age_fade * arrive * alpha));
            }
        }
    }

    unsafe fn paint_prompt(&self, width: f32, height: f32, prompt: &Prompt, frame: &Frame, alpha: f32) {
        unsafe {
            let fonts = self.fonts.as_ref().unwrap();
            let l = prompt::layout(width, height, prompt);
            let u = l.unit;

            if let Some(icon) = &l.icon {
                self.outline(icon, icon.width() / 2.0, tint(RED, 0.9 * alpha), 2.0 * u);
                self.text("!", &fonts.glyph, icon, tint(RED, alpha));
            }
            self.text(&prompt.title, &fonts.heading, &l.title, white(0.96 * alpha));
            self.text(&prompt.body, &fonts.message, &l.body, white(0.78 * alpha));

            if let (Some(rect), Some(code)) = (&l.code, &prompt.code) {
                self.fill(rect, 16.0 * u, white(0.06 * alpha));
                self.outline(rect, 16.0 * u, tint(ACCENT, 0.45 * alpha), 1.0);
                self.text(&spaced(code), &fonts.code, rect, white(0.98 * alpha));
            }
            if let (Some(rect), Some(qr)) = (&l.qr, &frame.qr) {
                self.paint_qr(rect, qr, u, alpha);
            }
            if let (Some(rect), Some(status)) = (&l.status, &prompt.status) {
                let text_width = status.chars().count() as f32 * 7.6 * u;
                let cx = width / 2.0 - text_width / 2.0 - 18.0 * u;
                self.spinner(cx, (rect.top + rect.bottom) / 2.0, 8.0 * u, 1.3 * u, alpha);
                self.text(status, &fonts.small_centred, rect, white(0.6 * alpha));
            }

            for (index, (rect, (name, detail))) in l.list.iter().zip(&prompt.list).enumerate() {
                self.fill(rect, 14.0 * u, white(0.07 * alpha));
                self.outline(rect, 14.0 * u, white(0.10 * alpha), 1.0);
                let name_line = Rect { left: rect.left + 22.0 * u, top: rect.top + 8.0 * u, bottom: rect.top + 38.0 * u, ..*rect };
                let detail_line = Rect { top: name_line.bottom, bottom: rect.bottom - 8.0 * u, ..name_line };
                self.text(name, &fonts.label, &name_line, white(0.95 * alpha));
                self.text(detail, &fonts.small, &detail_line, white(0.55 * alpha));
                if frame.focus == Some(index) {
                    self.focus_ring(rect, u, alpha);
                }
            }

            for (index, (rect, button)) in l.buttons.iter().zip(&prompt.buttons).enumerate() {
                let armed = frame.armed == Some(Armed::PromptButton(index));
                let copied = button.role == prompt::Role::Copy && frame.copied;
                if copied {
                    self.fill(rect, 14.0 * u, tint(ACCENT, 0.18 * alpha));
                    self.outline(rect, 14.0 * u, tint(ACCENT, 0.55 * alpha), 1.0);
                    self.text(text().copied, &fonts.button, rect, tint(ACCENT, alpha));
                } else {
                    let label = if armed { text().confirm } else { button.label };
                    self.button(rect, label, button.style, armed, u, alpha);
                }
                if frame.focus == Some(prompt.list.len() + index) {
                    self.focus_ring(rect, u, alpha);
                }
            }
        }
    }

    /// Dark modules on a white card; neighbours in a row are drawn as one bar.
    unsafe fn paint_qr(&self, card: &Rect, qr: &QrCode, u: f32, alpha: f32) {
        unsafe {
            self.fill(card, 10.0 * u, white(alpha));
            const QUIET_MODULES: f32 = 3.0;
            let module = card.width() / (qr.side as f32 + 2.0 * QUIET_MODULES);
            let origin = (card.left + QUIET_MODULES * module, card.top + QUIET_MODULES * module);
            for (row, modules) in qr.dark.chunks(qr.side).enumerate() {
                let mut column = 0;
                while column < qr.side {
                    if !modules[column] {
                        column += 1;
                        continue;
                    }
                    let start = column;
                    while column < qr.side && modules[column] {
                        column += 1;
                    }
                    let bar = Rect {
                        left: origin.0 + start as f32 * module,
                        top: origin.1 + row as f32 * module,
                        right: origin.0 + column as f32 * module,
                        // A hair of overlap hides seams between rows.
                        bottom: origin.1 + (row + 1) as f32 * module + 0.3,
                    };
                    self.fill(&bar, 0.0, rgba(0.0, 0.0, 0.0, alpha));
                }
            }
        }
    }

    unsafe fn paint_menu(&self, l: &Layout, openness: f32, settings: &Settings, frame: &Frame) {
        unsafe {
            let fonts = self.fonts.as_ref().unwrap();
            let t = text();
            let u = l.unit;
            let dx = (1.0 - openness) * l.sheet.width();
            let a = openness;
            let at = |rect: &Rect| rect.shifted(dx);

            // Body, with a soft shadow on its leading edge.
            for (i, shade) in [0.03f32, 0.05, 0.08, 0.12].iter().enumerate() {
                let x = l.sheet.left + dx - (4 - i) as f32 * 5.0 * u;
                let strip = Rect { left: x, top: 0.0, right: x + 5.0 * u, bottom: l.sheet.bottom };
                self.fill(&strip, 0.0, rgba(0.0, 0.0, 0.0, shade * a));
            }
            self.fill(&at(&l.sheet), 0.0, rgba(0.047, 0.055, 0.071, 0.97 * a));
            let edge = Rect { left: l.sheet.left + dx, top: 0.0, right: l.sheet.left + dx + 1.0, bottom: l.sheet.bottom };
            self.fill(&edge, 0.0, white(0.10 * a));

            self.text(t.settings, &fonts.title, &at(&l.title), white(0.96 * a));
            self.fill(&at(&l.close), 14.0 * u, white(0.07 * a));
            self.text("\u{2715}", &fonts.button, &at(&l.close), white(0.75 * a));

            for (index, (rect, tab)) in l.tabs.iter().zip(&TABS).enumerate() {
                let rect = at(rect);
                let name = (tab.name)(t);
                if index == frame.tab {
                    self.fill(&rect, 12.0 * u, tint(ACCENT, 0.16 * a));
                    let underline = Rect { top: rect.bottom - 3.0 * u, left: rect.left + 14.0 * u, right: rect.right - 14.0 * u, ..rect };
                    self.fill(&underline, 1.5 * u, tint(ACCENT, a));
                    self.text(name, &fonts.tab, &rect, white(0.98 * a));
                } else {
                    self.fill(&rect, 12.0 * u, white(0.05 * a));
                    self.text(name, &fonts.tab, &rect, white(0.62 * a));
                }
            }

            for (index, row) in l.rows.iter().enumerate() {
                // An action row has no value, so its description may use the whole line.
                let label = match row.item.kind() {
                    Kind::Action { .. } => Rect { right: row.value.right, ..row.label },
                    _ => row.label,
                };
                self.text(row.item.label(), &fonts.label, &at(&label), white(0.80 * a));
                self.text(&row.item.display(settings), &fonts.value, &at(&row.value), white(0.96 * a));
                let about = row.item.about(&frame.console_name);
                self.text(&about, &fonts.about, &at(&row.about), white(0.48 * a));
                match row.item.kind() {
                    Kind::Slider { .. } => self.paint_slider(row, settings, dx, u, a),
                    Kind::Choice(options) => {
                        let chosen = row.item.choice(settings);
                        for (option, (rect, name)) in row.options.iter().zip(options).enumerate() {
                            let rect = at(rect);
                            if option == chosen {
                                self.fill(&rect, 14.0 * u, tint(ACCENT, 0.20 * a));
                                self.outline(&rect, 14.0 * u, tint(ACCENT, 0.65 * a), 1.5 * u);
                                self.text(name, &fonts.button, &rect, white(0.98 * a));
                            } else {
                                self.button(&rect, name, Style::Plain, false, u, a * 0.75);
                            }
                        }
                    }
                    Kind::Action { confirm } => {
                        let armed = frame.armed == Some(Armed::MenuItem(row.item));
                        let label = if armed { t.confirm } else { row.item.action_label() };
                        let style = if confirm { Style::Warning } else { Style::Plain };
                        self.button(&at(&row.control), label, style, armed, u, a);
                    }
                }
                if frame.focus == Some(index) {
                    self.focus_ring(&at(&row.control), u, a);
                }
            }

            self.button(&at(&l.reset), t.defaults, Style::Plain, false, u, a);
            self.button(&at(&l.quit), t.quit, Style::Danger, false, u, a);
            for (offset, rect) in [&l.reset, &l.quit].into_iter().enumerate() {
                if frame.focus == Some(l.rows.len() + offset) {
                    self.focus_ring(&at(rect), u, a);
                }
            }
        }
    }

    /// Minus, a value bar with a tick at the default, and plus.
    unsafe fn paint_slider(&self, row: &menu::Row, settings: &Settings, dx: f32, u: f32, a: f32) {
        unsafe {
            let fonts = self.fonts.as_ref().unwrap();
            let item: Item = row.item;
            for (rect, glyph) in [(&row.minus, "\u{2212}"), (&row.plus, "+")] {
                let rect = rect.shifted(dx);
                self.fill(&rect, 14.0 * u, white(0.07 * a));
                self.outline(&rect, 14.0 * u, white(0.09 * a), 1.0);
                self.text(glyph, &fonts.glyph, &rect, white(0.92 * a));
            }
            let track = row.track.shifted(dx);
            let middle = (track.top + track.bottom) / 2.0;
            let bar = Rect { left: track.left, top: middle - 3.0 * u, right: track.right, bottom: middle + 3.0 * u };
            self.fill(&bar, 3.0 * u, white(0.12 * a));
            let filled = item.fraction(item.value(settings));
            if filled > 0.0 {
                let fill = Rect { right: bar.left + (bar.width() * filled).max(6.0 * u), ..bar };
                self.fill(&fill, 3.0 * u, tint(ACCENT, a));
            }
            let default_x = bar.left + bar.width() * item.fraction(item.value(&Settings::default()));
            let tick = Rect { left: default_x - 1.0, top: middle - 9.0 * u, right: default_x + 1.0, bottom: middle + 9.0 * u };
            self.fill(&tick, 0.0, white(0.38 * a));
            self.dot(bar.left + bar.width() * filled, middle, 8.0 * u, white(0.98 * a));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_animation_ends_fully_open_or_closed() {
        assert_eq!(menu_openness(true, None), 1.0);
        assert_eq!(menu_openness(false, None), 0.0);
        let just_now = Some(Instant::now());
        assert!(menu_openness(true, just_now) < 0.5, "starts closed when opening");
        assert!(menu_openness(false, just_now) > 0.5, "starts open when closing");
        assert_eq!(ease_out(0.0), 0.0);
        assert_eq!(ease_out(2.0), 1.0);
    }

    #[test]
    fn the_wordmark_is_letter_spaced_capitals() {
        assert_eq!(spaced("Ab"), "A\u{2009}B");
    }
}
