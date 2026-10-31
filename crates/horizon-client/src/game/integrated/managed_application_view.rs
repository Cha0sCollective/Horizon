//! Presentation for the isolated frozen application. This module owns no session
//! resources and returns input intent only; the application rechecks admission.

use horizon_client::{
    hud::{HudBatch, rgba},
    text::Text,
    ui,
    widgets::{self, Cursor},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FrozenViewAction {
    Close,
    Retry,
}

/// Plain observations supplied by the lifetime owner, never queried by the view.
pub(super) struct FrozenViewModel<'a> {
    pub phase: &'a str,
    pub status: &'a str,
    pub close_attempt: Option<u64>,
    pub failed_attempt: Option<u64>,
    pub owner_facts: &'a str,
    pub guest_facts: &'a str,
    pub counts: &'a str,
    pub clocks: &'a str,
    pub diagnostic: &'a str,
    pub close_allowed: bool,
    pub retry_allowed: bool,
}

pub(super) struct FrozenViewFrame {
    pub hud: HudBatch,
    pub cursor: Cursor,
}

#[derive(Clone, Copy)]
struct Hitbox {
    action: FrozenViewAction,
    rect: [f32; 4],
}

/// Replaceable presentation state. It cannot retain a game, worker or renderer.
#[derive(Default)]
pub(super) struct FrozenView {
    hitboxes: [Option<Hitbox>; 2],
    scale: f32,
    frames: u64,
}

impl FrozenView {
    pub(super) fn clear_hitboxes(&mut self) {
        self.hitboxes = [None; 2];
    }

    /// `pointer` and hit testing use physical window pixels, like native events.
    pub(super) fn action_at(&self, pointer: (f32, f32)) -> Option<FrozenViewAction> {
        if self.scale <= 0.0 || !pointer.0.is_finite() || !pointer.1.is_finite() {
            return None;
        }
        let pointer = (pointer.0 / self.scale, pointer.1 / self.scale);
        self.hitboxes.iter().flatten().find_map(|hitbox| {
            let [x, y, w, h] = hitbox.rect;
            ui::hovered(pointer, x, y, w, h).then_some(hitbox.action)
        })
    }

    pub(super) fn draw(
        &mut self,
        gui: &crate::game::UiResources,
        dimensions: [u32; 2],
        pointer: Option<(f32, f32)>,
        model: &FrozenViewModel<'_>,
    ) -> FrozenViewFrame {
        // Do not inherit cursor requests from any previous presentation.
        let _ = widgets::take_cursor();
        self.clear_hitboxes();
        let mut hud = HudBatch::new(dimensions);
        self.scale = hud.scale;
        let pointer = pointer
            .filter(|(x, y)| x.is_finite() && y.is_finite())
            .map(|(x, y)| (x / hud.scale, y / hud.scale));
        let (width, height) = (hud.width, hud.height);
        if width > 0.0 && height > 0.0 {
            hud.fill(&gui.atlas, 0.0, 0.0, width, height, rgba(13, 18, 25, 255));
        }
        if width >= 48.0 && height >= 48.0 {
            self.panel(&mut hud, gui, pointer, model);
            hud.clip_since(0, 0.0, 0.0, width, height);
        }
        self.frames = self.frames.wrapping_add(1);
        widgets::end_frame(self.frames);
        FrozenViewFrame {
            hud,
            cursor: widgets::take_cursor(),
        }
    }

    fn panel(
        &mut self,
        hud: &mut HudBatch,
        gui: &crate::game::UiResources,
        pointer: Option<(f32, f32)>,
        model: &FrozenViewModel<'_>,
    ) {
        let atlas = &gui.atlas;
        let panel_width = (hud.width - 16.0).min(480.0);
        let panel_height = (hud.height - 16.0).min(240.0);
        let x = ((hud.width - panel_width) / 2.0).floor();
        let y = ((hud.height - panel_height) / 2.0).floor();
        hud.fill(
            atlas,
            x,
            y,
            panel_width,
            panel_height,
            rgba(45, 54, 68, 255),
        );
        hud.fill(
            atlas,
            x + 1.0,
            y + 1.0,
            panel_width - 2.0,
            panel_height - 2.0,
            rgba(24, 31, 42, 255),
        );
        let left = x + 8.0;
        let right = x + panel_width - 8.0;
        let button_y = y + panel_height - 28.0;
        let content_bottom = button_y - 6.0;
        let line = |hud: &mut HudBatch, row: usize, label: &str, value: &str, color| {
            let top = y + 28.0 + row as f32 * 14.0;
            if top + 10.0 <= content_bottom {
                draw_line(
                    hud,
                    atlas,
                    [left, top, right, top + 10.0],
                    label,
                    value,
                    color,
                );
            }
        };
        if y + 18.0 <= content_bottom {
            draw_line(
                hud,
                atlas,
                [left, y + 9.0, right, y + 19.0],
                "",
                "Isolated frozen recovery",
                rgba(245, 247, 250, 255),
            );
        }
        let bright = rgba(225, 232, 240, 255);
        let muted = rgba(180, 194, 210, 255);
        line(hud, 0, "Phase: ", model.phase, bright);
        line(hud, 1, "Status: ", model.status, bright);
        let attempt = model
            .close_attempt
            .map_or_else(|| "none".into(), |n| n.to_string());
        line(hud, 2, "Close attempt: ", &attempt, muted);
        let failed = model
            .failed_attempt
            .map_or_else(|| "none".into(), |n| n.to_string());
        line(hud, 3, "Failed save attempt: ", &failed, muted);
        line(hud, 4, "MemoryOwner: ", model.owner_facts, muted);
        line(hud, 5, "MemoryGuest: ", model.guest_facts, muted);
        line(hud, 6, "Counts: ", model.counts, muted);
        line(hud, 7, "Clocks: ", model.clocks, muted);
        line(hud, 8, "Detail: ", model.diagnostic, muted);
        line(
            hud,
            9,
            "",
            "Frozen status only; no world scene or animation.",
            muted,
        );
        let permitted = [
            (FrozenViewAction::Close, "Close", model.close_allowed),
            (FrozenViewAction::Retry, "Retry", model.retry_allowed),
        ];
        let count = permitted.iter().filter(|(_, _, allowed)| *allowed).count();
        let available = right - left;
        let button_width = if count > 1 {
            ((available - 8.0) / 2.0).min(120.0)
        } else {
            available.min(120.0)
        };
        // A small/minimized drawable must never retain a hidden active target.
        if count == 0 || button_width < 48.0 || button_y < y + 24.0 {
            return;
        }
        let mut at = 0;
        for (action, label, allowed) in permitted {
            if !allowed {
                continue;
            }
            let bx = left + at as f32 * (button_width + 8.0);
            let rect = [bx, button_y, button_width, 20.0];
            let hover = pointer.is_some_and(|p| ui::hovered(p, bx, button_y, button_width, 20.0));
            ui::button(
                hud,
                atlas,
                bx,
                button_y,
                button_width,
                20.0,
                &Text::literal(label),
                true,
                hover,
            );
            self.hitboxes[at] = Some(Hitbox { action, rect });
            at += 1;
        }
    }
}

/// Bound work by Unicode scalar count without slicing within UTF-8. The font
/// resolves legacy section codes even for Text::literal, so render their marker
/// as a question mark instead of allowing diagnostics to change style.
fn bounded_text(value: &str) -> String {
    value
        .chars()
        .take(256)
        .map(|c| {
            if c.is_control() || c == '\u{a7}' {
                '?'
            } else {
                c
            }
        })
        .collect()
}

fn draw_line(
    hud: &mut HudBatch,
    atlas: &horizon_client::hud::HudAtlas,
    clip: [f32; 4],
    label: &str,
    value: &str,
    color: u32,
) {
    let [x0, y0, x1, y1] = clip;
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    let text = Text::literal(format!("{label}{}", bounded_text(value)));
    let start = hud.vertices.len();
    hud.text_styled(atlas, &text, x0, y0, color, false);
    hud.clip_since(start, x0, y0, x1, y1);
}
