//! Look & feel: a dark theme with Inter for text, JetBrains Mono for times and
//! Material Symbols Rounded for icons (Apache-2.0 / OFL fonts bundled in `assets/fonts`).

use egui::epaint::text::{FontData, FontDefinitions, FontTweak};
use egui::{
    Color32, CornerRadius, FontFamily, FontId, Margin, Response, RichText, Shadow, Stroke,
    TextStyle, Ui, vec2,
};
use std::sync::Arc;

// Palette.
pub const BG_APP: Color32 = Color32::from_rgb(0x14, 0x15, 0x18);
pub const BG_PANEL: Color32 = Color32::from_rgb(0x1b, 0x1c, 0x20);
pub const BG_RAISED: Color32 = Color32::from_rgb(0x24, 0x25, 0x2b);
pub const BG_HOVER: Color32 = Color32::from_rgb(0x2e, 0x30, 0x37);
pub const BG_INPUT: Color32 = Color32::from_rgb(0x10, 0x11, 0x14);
pub const BORDER: Color32 = Color32::from_rgb(0x32, 0x34, 0x3b);
pub const TEXT: Color32 = Color32::from_rgb(0xe8, 0xe8, 0xeb);
pub const TEXT_DIM: Color32 = Color32::from_rgb(0x9b, 0x9d, 0xa6);
pub const TEXT_FAINT: Color32 = Color32::from_rgb(0x66, 0x68, 0x71);
pub const ACCENT: Color32 = Color32::from_rgb(0x4a, 0x7c, 0xf0);
pub const GO: Color32 = Color32::from_rgb(0x2a, 0xa8, 0x4e);
pub const GREEN: Color32 = Color32::from_rgb(0x3c, 0xc8, 0x64);
pub const AMBER: Color32 = Color32::from_rgb(0xf2, 0xb0, 0x1e);
pub const RED: Color32 = Color32::from_rgb(0xe8, 0x4a, 0x4f);
pub const BLUE: Color32 = Color32::from_rgb(0x5b, 0x9b, 0xf5);

pub fn accent_bg() -> Color32 {
    Color32::from_rgba_unmultiplied(0x4a, 0x7c, 0xf0, 60)
}

const SEMIBOLD: &str = "semibold";
const ICONS: &str = "icons";
const ICONS_FILLED: &str = "icons-filled";
const TITLE: &str = "title";
/// Downward shift (fraction of font size) for icons mixed into Inter text.
const INLINE_ICON_OFFSET: f32 = 0.2;

/// Condensed display face for the app title.
pub fn title(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(TITLE.into()))
}

pub fn semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(SEMIBOLD.into()))
}

pub fn icon_font(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(ICONS.into()))
}

pub fn icon_font_filled(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(ICONS_FILLED.into()))
}

pub fn mono(size: f32) -> FontId {
    FontId::monospace(size)
}

/// An icon as text (outlined style).
pub fn icon(glyph: &str, size: f32) -> RichText {
    RichText::new(glyph).font(icon_font(size))
}

/// "icon  label" for buttons and menu items (icons render inline through font fallback).
pub fn with_icon(glyph: &str, label: &str) -> String {
    format!("{glyph}  {label}")
}

/// A compact, frameless icon button with hover highlight.
pub fn icon_button(ui: &mut Ui, glyph: &str, tooltip: &str) -> Response {
    ui.add(
        egui::Button::new(icon(glyph, 20.0))
            .frame(false)
            .min_size(vec2(30.0, 30.0)),
    )
    .on_hover_text(tooltip)
}

pub fn install(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let add = |fonts: &mut FontDefinitions, name: &str, bytes: &'static [u8], tweak: FontTweak| {
        fonts.font_data.insert(
            name.to_owned(),
            Arc::new(FontData::from_static(bytes).tweak(tweak)),
        );
    };
    add(
        &mut fonts,
        "Inter",
        include_bytes!("../../assets/fonts/Inter-Regular.ttf"),
        FontTweak::default(),
    );
    add(
        &mut fonts,
        "Inter-SemiBold",
        include_bytes!("../../assets/fonts/Inter-SemiBold.ttf"),
        FontTweak::default(),
    );
    add(
        &mut fonts,
        "JetBrainsMono",
        include_bytes!("../../assets/fonts/JetBrainsMono-Medium.ttf"),
        FontTweak::default(),
    );
    // Material Symbols glyphs are centred in their own line box, so standalone icons need no
    // tweak. When an icon is mixed into Inter text it sits on Inter's baseline, where its centre
    // is ~0.21 em above Inter's cap-height centre: shift those down to line up with the text.
    let outlined: &'static [u8] = include_bytes!("../../assets/fonts/MaterialSymbolsRounded.ttf");
    let filled: &'static [u8] =
        include_bytes!("../../assets/fonts/MaterialSymbolsRounded-Filled.ttf");
    let inline = FontTweak {
        scale: 1.15,
        y_offset_factor: INLINE_ICON_OFFSET,
        ..Default::default()
    };
    add(&mut fonts, "SymbolsInline", outlined, inline);
    add(&mut fonts, "Symbols", outlined, FontTweak::default());
    add(&mut fonts, "SymbolsFilled", filled, FontTweak::default());
    add(
        &mut fonts,
        "BarlowCondensed",
        include_bytes!("../../assets/fonts/BarlowCondensed-SemiBold.ttf"),
        FontTweak::default(),
    );

    let defaults_prop = fonts
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    let defaults_mono = fonts
        .families
        .get(&FontFamily::Monospace)
        .cloned()
        .unwrap_or_default();
    let chain = |first: &[&str], rest: &[String]| -> Vec<String> {
        first
            .iter()
            .map(|s| s.to_string())
            .chain(rest.iter().cloned())
            .collect()
    };
    fonts.families.insert(
        FontFamily::Proportional,
        chain(&["Inter", "SymbolsInline"], &defaults_prop),
    );
    fonts.families.insert(
        FontFamily::Monospace,
        chain(&["JetBrainsMono", "SymbolsInline"], &defaults_mono),
    );
    fonts.families.insert(
        FontFamily::Name(SEMIBOLD.into()),
        chain(&["Inter-SemiBold", "SymbolsInline"], &defaults_prop),
    );
    fonts.families.insert(
        FontFamily::Name(TITLE.into()),
        chain(&["BarlowCondensed"], &defaults_prop),
    );
    fonts
        .families
        .insert(FontFamily::Name(ICONS.into()), vec!["Symbols".into()]);
    fonts.families.insert(
        FontFamily::Name(ICONS_FILLED.into()),
        vec!["SymbolsFilled".into()],
    );
    ctx.set_fonts(fonts);

    ctx.set_theme(egui::Theme::Dark);
    ctx.style_mut_of(egui::Theme::Dark, |style| {
        style.text_styles = [
            (TextStyle::Small, FontId::proportional(11.5)),
            (TextStyle::Body, FontId::proportional(14.0)),
            (TextStyle::Button, FontId::proportional(13.5)),
            (TextStyle::Heading, semibold(18.0)),
            (TextStyle::Monospace, mono(13.0)),
        ]
        .into();
        let sp = &mut style.spacing;
        sp.item_spacing = vec2(8.0, 6.0);
        sp.button_padding = vec2(10.0, 5.0);
        sp.interact_size.y = 26.0;
        sp.menu_margin = Margin::same(8);
        sp.window_margin = Margin::same(12);
        sp.combo_width = 140.0;
        sp.slider_width = 160.0;
        sp.scroll.bar_width = 8.0;

        let v = &mut style.visuals;
        v.dark_mode = true;
        v.override_text_color = None;
        v.panel_fill = BG_PANEL;
        v.window_fill = BG_RAISED;
        v.extreme_bg_color = BG_INPUT;
        v.faint_bg_color = Color32::from_rgb(0x18, 0x19, 0x1d);
        v.code_bg_color = BG_INPUT;
        v.window_stroke = Stroke::new(1.0, BORDER);
        v.window_corner_radius = CornerRadius::same(10);
        v.menu_corner_radius = CornerRadius::same(8);
        v.window_shadow = Shadow {
            offset: [0, 8],
            blur: 24,
            spread: 0,
            color: Color32::from_black_alpha(120),
        };
        v.popup_shadow = Shadow {
            offset: [0, 6],
            blur: 16,
            spread: 0,
            color: Color32::from_black_alpha(110),
        };
        v.selection.bg_fill = accent_bg();
        v.selection.stroke = Stroke::new(1.0, ACCENT);
        v.hyperlink_color = BLUE;
        v.warn_fg_color = AMBER;
        v.error_fg_color = RED;
        v.striped = false;

        let r = CornerRadius::same(6);
        let w = &mut v.widgets;
        w.noninteractive.bg_fill = BG_PANEL;
        w.noninteractive.weak_bg_fill = BG_PANEL;
        w.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
        w.noninteractive.fg_stroke = Stroke::new(1.0, TEXT_DIM);
        w.noninteractive.corner_radius = r;
        w.inactive.bg_fill = BG_RAISED;
        w.inactive.weak_bg_fill = BG_RAISED;
        w.inactive.bg_stroke = Stroke::new(1.0, BORDER);
        w.inactive.fg_stroke = Stroke::new(1.0, TEXT);
        w.inactive.corner_radius = r;
        w.hovered.bg_fill = BG_HOVER;
        w.hovered.weak_bg_fill = BG_HOVER;
        w.hovered.bg_stroke = Stroke::new(1.0, Color32::from_rgb(0x48, 0x4b, 0x55));
        w.hovered.fg_stroke = Stroke::new(1.0, Color32::WHITE);
        w.hovered.corner_radius = r;
        w.hovered.expansion = 0.0;
        w.active.bg_fill = Color32::from_rgb(0x36, 0x39, 0x42);
        w.active.weak_bg_fill = Color32::from_rgb(0x36, 0x39, 0x42);
        w.active.bg_stroke = Stroke::new(1.0, ACCENT);
        w.active.fg_stroke = Stroke::new(1.0, Color32::WHITE);
        w.active.corner_radius = r;
        w.active.expansion = 0.0;
        w.open.bg_fill = BG_HOVER;
        w.open.weak_bg_fill = BG_HOVER;
        w.open.bg_stroke = Stroke::new(1.0, BORDER);
        w.open.corner_radius = r;
    });
}

/// Icon for each cue type.
pub fn kind_icon(tag: crate::model::KindTag) -> &'static str {
    use super::icons as I;
    use crate::model::KindTag::*;
    match tag {
        Audio => I::VOLUME_UP,
        Video => I::MOVIE,
        Group => I::FOLDER,
        Wait => I::HOURGLASS_EMPTY,
        Memo => I::STICKY_NOTE_2,
        Fade => I::TRENDING_DOWN,
        Start => I::PLAY_CIRCLE,
        Stop => I::STOP_CIRCLE,
        Pause => I::PAUSE_CIRCLE,
        Goto => I::SUBDIRECTORY_ARROW_RIGHT,
        Devamp => I::SKIP_NEXT,
        Network => I::LAN,
        Text => I::TEXT_FIELDS,
    }
}

/// Section caption in small caps style ("STANDBY", "ACTIVE CUES").
pub fn caption(text: &str) -> RichText {
    RichText::new(text.to_uppercase())
        .font(semibold(10.5))
        .color(TEXT_FAINT)
}
