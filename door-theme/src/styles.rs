//! The shared iced widget styles for door's auth surfaces — the glassy card,
//! the slim input fields, and the accent button the greeter and the lock screen
//! both render. One source of truth so "the lock screen looks exactly like the
//! login screen" stays a property of the code, not a discipline.

use iced::widget::{button, container, text_input};
use iced::{Background, Border, Shadow, Vector};

use crate::{Color, Theme};

/// Lighten a color toward white by `amt` (0.0–1.0) — for button hover.
pub fn lighten(c: Color, amt: f32) -> Color {
    let mix = |v: u8| (v as f32 + (255.0 - v as f32) * amt).round() as u8;
    Color {
        r: mix(c.r),
        g: mix(c.g),
        b: mix(c.b),
        a: c.a,
    }
}

/// Slim rounded input: a lifted field fill with an accent border on focus.
pub fn field_style(
    t: &Theme,
    fade: f32,
) -> impl Fn(&iced::Theme, text_input::Status) -> text_input::Style {
    let field = t.field.iced_alpha(fade);
    let fg = t.foreground.iced_alpha(fade);
    let muted = t.muted.iced_alpha(fade);
    let accent = t.accent.iced_alpha(fade);
    let radius = t.field_radius;
    move |_theme, status| {
        let focused = matches!(status, text_input::Status::Focused { .. });
        text_input::Style {
            background: Background::Color(field),
            border: Border {
                radius: radius.into(),
                width: 1.0,
                color: if focused {
                    accent
                } else {
                    iced::Color::TRANSPARENT
                },
            },
            icon: muted,
            placeholder: muted,
            value: fg,
            selection: accent,
        }
    }
}

/// The accent action button (sign in / unlock), brighter on hover, with dark text.
pub fn button_style(
    t: &Theme,
    fade: f32,
) -> impl Fn(&iced::Theme, button::Status) -> button::Style {
    let accent = t.accent;
    let radius = t.field_radius;
    let on_accent = Color {
        r: 0x16,
        g: 0x16,
        b: 0x1e,
        a: 0xff,
    };
    move |_theme, status| {
        let bg = match status {
            button::Status::Hovered | button::Status::Pressed => lighten(accent, 0.15),
            _ => accent,
        };
        button::Style {
            background: Some(Background::Color(bg.iced_alpha(fade))),
            text_color: on_accent.iced_alpha(fade),
            border: Border {
                radius: radius.into(),
                ..Default::default()
            },
            ..Default::default()
        }
    }
}

/// The card fill: a flat color, or — when `gradient` > 0 — a subtle vertical sheen
/// whose top edge lightens toward white and eases down to the card color.
pub fn card_background(card: iced::Color, gradient: f32) -> Background {
    if gradient <= 0.0 {
        return Background::Color(card);
    }
    let amt = 0.18 * gradient.clamp(0.0, 1.0);
    let lit = |c: f32| c + (1.0 - c) * amt;
    let top = iced::Color {
        r: lit(card.r),
        g: lit(card.g),
        b: lit(card.b),
        a: card.a,
    };
    Background::Gradient(iced::Gradient::Linear(
        iced::gradient::Linear::new(iced::Radians(std::f32::consts::PI))
            .add_stop(0.0, top)
            .add_stop(1.0, card),
    ))
}

/// The glassy card: translucent fill, a gently breathing accent hairline, soft
/// drop shadow. `phase` is the shared animation clock driving the breathing.
pub fn card_style(t: &Theme, fade: f32, phase: f32) -> impl Fn(&iced::Theme) -> container::Style {
    let card = t.card.iced_alpha(fade);
    let accent = t.accent;
    let radius = t.corner_radius;
    let shadow_opacity = t.card_shadow_opacity;
    let shadow_blur = t.card_shadow_blur;
    let gradient = t.card_gradient;
    // The accent hairline breathes between ~0.18 and ~0.34 alpha (speed × control).
    let breathe = 0.18 + 0.16 * (0.5 + 0.5 * (phase * 1.1 * t.accent_breathing).sin());
    move |_theme| container::Style {
        background: Some(card_background(card, gradient)),
        border: Border {
            radius: radius.into(),
            width: 1.0,
            color: accent.iced_alpha(fade * breathe),
        },
        shadow: Shadow {
            color: iced::Color::from_rgba(0.0, 0.0, 0.0, shadow_opacity * fade),
            offset: Vector::new(0.0, 10.0),
            blur_radius: shadow_blur,
        },
        ..Default::default()
    }
}
