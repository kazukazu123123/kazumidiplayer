use std::collections::HashMap;

use raylib::prelude::*;

use crate::window_manager::{self, WindowManager};

// Stack layout, modeled on Windows 7 Flip 3D. Lengths are fractions of the
// screen height so the layout keeps its shape at any resolution.
// Focal length of the camera.
const FOCAL: f32 = 1.2;
// Eye level. The front card's top edge sits on it, so that edge stays flat.
const HORIZON: f32 = 0.315;
// Front card center, relative to the screen center (x) and the horizon (y, up).
const FRONT_X: f32 = 0.28;
const FRONT_Y: f32 = -0.215;
// Each card further back moves this far left and away (as a fraction of the
// front card's distance from the camera).
const STEP_X: f32 = -0.29;
const STEP_Z: f32 = 0.17;
// Cards rise as they go back, leveling off at RISE.
const RISE: f32 = 0.24;
const RISE_FALLOFF: f32 = 1.2;
// Cards turn so their left edge faces the camera.
const YAW_DEGREES: f32 = 18.0;
// Windows shrink to this fraction of their desktop size and keep their sizes
// relative to each other. The largest card is capped at CARD_MAX_HEIGHT screen
// heights and CARD_MAX_WIDTH screen widths so oversized windows still fit.
const WINDOW_SCALE: f32 = 0.6;
const CARD_MAX_HEIGHT: f32 = 0.5;
const CARD_MAX_WIDTH: f32 = 0.5;
// Cards past this many slots behind the front fade out.
const VISIBLE_DEPTH: f32 = 7.0;

// Brightness of cards away from the front, as a fraction of full.
const UNSELECTED_BRIGHTNESS: f32 = 0.5;

const OPEN_SECONDS: f32 = 0.45;
const CLOSE_SECONDS: f32 = 0.4;
const SCROLL_SPEED: f32 = 12.0;
// Each card is split into a grid so the texture stays perspective correct.
// A single quad is drawn as two affine triangles and visibly kinks along the diagonal.
const GRID: usize = 8;

fn ease_out_cubic(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

fn ease_in_out_cubic(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    if t < 0.5 {
        4.0 * t * t * t
    } else {
        1.0 - (-2.0 * t + 2.0).powi(3) * 0.5
    }
}

fn lerp_vec(a: Vector2, b: Vector2, t: f32) -> Vector2 {
    Vector2::new(a.x + (b.x - a.x) * t, a.y + (b.y - a.y) * t)
}

#[derive(Clone, Copy, PartialEq)]
enum Phase {
    Closed,
    Open,
    Closing,
}

pub struct Flip3D {
    phase: Phase,
    order: Vec<i32>,
    // Unbounded slot count of the card in front. The selected card is
    // `target` modulo the card count; `scroll` chases it so cycling animates.
    target: i64,
    scroll: f32,
    reveal: f32,
    // Projected card outlines from the last draw, front to back, for mouse picking.
    hit_quads: Vec<(usize, [Vector2; 4])>,
}

impl Flip3D {
    pub fn new() -> Self {
        Self {
            phase: Phase::Closed,
            order: Vec::new(),
            target: 0,
            scroll: 0.0,
            reveal: 0.0,
            hit_quads: Vec::new(),
        }
    }

    /// True while Flip 3D is on screen, including the closing animation.
    pub fn is_active(&self) -> bool {
        self.phase != Phase::Closed
    }

    /// True while Flip 3D accepts input. The closing animation does not.
    pub fn is_interactive(&self) -> bool {
        self.phase == Phase::Open
    }

    fn selected_index(&self) -> Option<usize> {
        if self.order.is_empty() {
            return None;
        }
        Some(self.target.rem_euclid(self.order.len() as i64) as usize)
    }

    pub fn selected_id(&self) -> Option<i32> {
        self.selected_index().map(|index| self.order[index])
    }

    pub fn open(&mut self, window_manager: &WindowManager, direction: i32) {
        let mut ids = window_manager.get_window_ids_sorted();
        // Locked windows are background layers, not switchable windows.
        ids.retain(|id| {
            window_manager
                .get_window(*id)
                .map_or(false, |window| !window.background)
        });
        if ids.is_empty() {
            return;
        }
        // get_window_ids_sorted returns back-to-front; Flip 3D talks front-to-back.
        ids.reverse();

        // The stack opens in z-order with the active window in front, then
        // scrolls one step so the next window comes forward.
        let active_index = ids
            .iter()
            .position(|id| {
                window_manager
                    .get_window(*id)
                    .map_or(false, |window| window.is_active)
            })
            .unwrap_or(0);

        // Reopening during the closing animation keeps the current reveal so
        // the cards turn around instead of snapping back to the desktop.
        if self.phase == Phase::Closed {
            self.reveal = 0.0;
        }
        self.order = ids;
        self.target = active_index as i64;
        self.scroll = self.target as f32;
        self.hit_quads.clear();
        self.phase = Phase::Open;
        self.cycle(direction);
    }

    pub fn cycle(&mut self, direction: i32) {
        if !self.is_interactive() || self.order.is_empty() {
            return;
        }
        self.target += if direction < 0 { -1 } else { 1 };
    }

    /// Brings the selected window to the front and animates back to the desktop.
    pub fn commit(&mut self, window_manager: &mut WindowManager) {
        if !self.is_interactive() {
            return;
        }
        if let Some(id) = self.selected_id() {
            window_manager.activate_window(id);
        }
        self.phase = Phase::Closing;
    }

    /// Animates back to the desktop without changing the active window.
    pub fn cancel(&mut self) {
        if self.phase == Phase::Open {
            self.phase = Phase::Closing;
        }
    }

    /// Returns the card under `point`, front-most first, as of the last draw.
    pub fn card_at(&self, point: Vector2) -> Option<usize> {
        self.hit_quads
            .iter()
            .find(|(_, quad)| point_in_quad(point, quad))
            .map(|(index, _)| *index)
    }

    /// Brings the card under `point` to the front and commits it. Returns false on a miss.
    pub fn click(&mut self, point: Vector2, window_manager: &mut WindowManager) -> bool {
        let count = self.order.len() as i64;
        match self.card_at(point) {
            Some(index) if self.is_interactive() && count > 0 => {
                // Step forward to the clicked card so it slides to the front.
                let ahead = (index as i64 - self.target).rem_euclid(count);
                self.target += ahead;
                self.commit(window_manager);
                true
            }
            _ => false,
        }
    }

    pub fn update(&mut self, dt: f32) {
        match self.phase {
            Phase::Closed => return,
            Phase::Open => self.reveal = (self.reveal + dt / OPEN_SECONDS).min(1.0),
            Phase::Closing => {
                self.reveal = (self.reveal - dt / CLOSE_SECONDS).max(0.0);
                if self.reveal <= 0.0 {
                    self.phase = Phase::Closed;
                    self.order.clear();
                    self.hit_quads.clear();
                    return;
                }
            }
        }

        let target = self.target as f32;
        self.scroll += (target - self.scroll) * (1.0 - (-dt * SCROLL_SPEED).exp());
        if (target - self.scroll).abs() < 0.001 {
            self.scroll = target;
        }
    }

    /// Slots to draw as (card index, depth, opacity). Depth 0 is the front.
    /// While scrolling, the card wrapping around is drawn twice: one copy slides
    /// out past the front and fades, the other fades in at the back.
    fn slots(&self) -> Vec<(usize, f32, f32)> {
        let count = self.order.len();
        let count_f = count as f32;
        let mut slots = Vec::with_capacity(count + 1);
        for index in 0..count {
            let depth = (index as f32 - self.scroll).rem_euclid(count_f);
            let wrap = depth - (count_f - 1.0);
            if count > 1 && wrap > 0.0 {
                slots.push((index, depth - count_f, wrap));
                slots.push((index, depth, 1.0 - wrap));
            } else {
                slots.push((index, depth, 1.0));
            }
        }
        slots
    }

    pub fn draw(
        &mut self,
        d: &mut impl RaylibDraw,
        window_manager: &WindowManager,
        content_rts: &HashMap<i32, RenderTexture2D>,
        content_rt_padding: &HashMap<i32, f32>,
        screen_width: i32,
        screen_height: i32,
    ) {
        self.hit_quads.clear();
        if !self.is_active() || self.order.is_empty() {
            return;
        }

        // Opening eases out so the stack settles; closing eases in and out so
        // the cards land softly on their desktop spots.
        let reveal = match self.phase {
            Phase::Closing => ease_in_out_cubic(self.reveal),
            _ => ease_out_cubic(self.reveal),
        };
        let camera = Camera {
            screen_width: screen_width as f32,
            screen_height: screen_height as f32,
        };

        // Crop the bend padding so cards show only the window itself.
        let card_size = |id: i32| -> Option<(f32, f32, f32)> {
            let rt = content_rts.get(&id)?;
            let padding = *content_rt_padding.get(&id).unwrap_or(&0.0);
            let w = rt.texture.width as f32;
            let h = rt.texture.height as f32 - 2.0 * padding;
            (w > 0.0 && h > 0.0).then_some((w, h, padding))
        };

        let mut max_w = 1.0f32;
        let mut max_h = 1.0f32;
        for id in &self.order {
            if let Some((w, h, _)) = card_size(*id) {
                max_w = max_w.max(w);
                max_h = max_h.max(h);
            }
        }
        let scale = WINDOW_SCALE
            .min(CARD_MAX_HEIGHT * camera.screen_height / max_h)
            .min(CARD_MAX_WIDTH * camera.screen_width / max_w);

        let mut slots = self.slots();
        // Farthest first so nearer cards cover the ones behind.
        slots.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        for (index, depth, opacity) in slots {
            let opacity = opacity * (VISIBLE_DEPTH - depth).clamp(0.0, 1.0);
            if opacity <= 0.0 {
                continue;
            }
            let id = self.order[index];
            let (rt, (card_w, card_h, padding)) = match (content_rts.get(&id), card_size(id)) {
                (Some(rt), Some(size)) => (rt, size),
                _ => continue,
            };
            let window = match window_manager.get_window(id) {
                Some(window) => window,
                None => continue,
            };

            let bt = window_manager::BORDER_THICKNESS as f32;
            let desktop_origin = Vector2::new(
                window.rectangle.left() as f32 - bt,
                window.rectangle.top() as f32 - bt,
            );
            let card = Card {
                width: card_w,
                height: card_h,
                scale,
                depth,
                desktop_origin,
                reveal,
            };

            let tex_h = rt.texture.height as f32;
            let tex_top = 1.0 - padding / tex_h;
            let tex_span = card_h / tex_h;

            let mut grid = [[Vector2::zero(); GRID + 1]; GRID + 1];
            for (row, points) in grid.iter_mut().enumerate() {
                for (col, point) in points.iter_mut().enumerate() {
                    *point =
                        card.point(&camera, col as f32 / GRID as f32, row as f32 / GRID as f32);
                }
            }

            // How close the card is to the front slot, so the brightness
            // follows the scroll animation instead of jumping.
            let focus = (1.0 - depth.abs()).clamp(0.0, 1.0);
            let brightness = 1.0 - (1.0 - UNSELECTED_BRIGHTNESS) * (1.0 - focus) * reveal;
            let shade = (255.0 * brightness) as u8;
            let tint = Color::new(shade, shade, shade, (255.0 * opacity) as u8);
            // Cards are drawn shrunk, so sample from mipmaps to keep fine detail
            // like the piano roll grid from shimmering. The window RTs are
            // redrawn every frame, so the mipmaps are rebuilt here each time and
            // the filter goes back to point sampling for the normal composite.
            let mut texture = rt.texture;
            // Safety: rlgl FFI; `texture` is a copy of the live GL texture owned by `rt`.
            unsafe {
                raylib::ffi::GenTextureMipmaps(&mut texture);
                raylib::ffi::SetTextureFilter(
                    texture,
                    TextureFilter::TEXTURE_FILTER_TRILINEAR as i32,
                );
                raylib::ffi::rlSetTexture(texture.id);
            }
            d.rl_draw(DrawMode::Quads, |vertices| {
                vertices.color4ub(tint);
                for row in 0..GRID {
                    let v0 = row as f32 / GRID as f32;
                    let v1 = (row + 1) as f32 / GRID as f32;
                    for col in 0..GRID {
                        let u0 = col as f32 / GRID as f32;
                        let u1 = (col + 1) as f32 / GRID as f32;
                        let quad = [
                            (u0, v0, grid[row][col]),
                            (u0, v1, grid[row + 1][col]),
                            (u1, v1, grid[row + 1][col + 1]),
                            (u1, v0, grid[row][col + 1]),
                        ];
                        for (u, v, point) in quad {
                            vertices.texcoord2f(u, tex_top - v * tex_span);
                            vertices.vertex2f(point.x, point.y);
                        }
                    }
                }
            });
            // rlgl batches draws, so flush before the filter changes back or the
            // card would be drawn with point sampling after all.
            // Release the texture through rlSetTexture, the batch-aware pair of
            // the call above. rl_disable_texture unbinds it in GL directly behind
            // rlgl's back, and the next frame's drawing into this window's RT
            // then loses its shapes, leaving only the title text.
            // Safety: as above.
            unsafe {
                raylib::ffi::rlSetTexture(0);
                raylib::ffi::rlDrawRenderBatchActive();
                raylib::ffi::SetTextureFilter(texture, TextureFilter::TEXTURE_FILTER_POINT as i32);
            }

            let corners = [grid[0][0], grid[0][GRID], grid[GRID][GRID], grid[GRID][0]];

            // Fading copies are mid-wrap and should not take clicks.
            if opacity >= 0.5 {
                self.hit_quads.push((index, corners));
            }
        }
        // Cards were drawn back to front; picking wants the front first.
        self.hit_quads.reverse();
    }
}

struct Camera {
    screen_width: f32,
    screen_height: f32,
}

impl Camera {
    /// Projects a point in camera space, measured in screen heights, with y up
    /// and the camera looking down negative z.
    fn project(&self, x: f32, y: f32, z: f32) -> Vector2 {
        let h = self.screen_height;
        let focal = FOCAL * h;
        let distance = (-z).max(0.01);
        Vector2::new(
            self.screen_width * 0.5 + focal * x / distance,
            HORIZON * h - focal * y / distance,
        )
    }
}

struct Card {
    width: f32,
    height: f32,
    scale: f32,
    depth: f32,
    desktop_origin: Vector2,
    reveal: f32,
}

impl Card {
    /// Screen position of the card point (u, v), blended from the flat desktop
    /// layout at reveal 0 to the stack at reveal 1.
    fn point(&self, camera: &Camera, u: f32, v: f32) -> Vector2 {
        let desktop = Vector2::new(
            self.desktop_origin.x + u * self.width,
            self.desktop_origin.y + v * self.height,
        );
        lerp_vec(desktop, self.stacked_point(camera, u, v), self.reveal)
    }

    fn stacked_point(&self, camera: &Camera, u: f32, v: f32) -> Vector2 {
        let h = camera.screen_height;
        let depth = self.depth;

        // Card center in camera space. Negative depth (a card sliding out past
        // the front) extrapolates the same path, forward and down to the right.
        let center_x = FRONT_X + STEP_X * depth;
        let center_y = FRONT_Y + RISE * (1.0 - (-depth / RISE_FALLOFF).exp());
        let center_z = -FOCAL * (1.0 + STEP_Z * depth);

        let half_w = self.width * self.scale / h * 0.5;
        let half_h = self.height * self.scale / h * 0.5;
        let x = (u * 2.0 - 1.0) * half_w;
        let y = (1.0 - v * 2.0) * half_h;

        let (sin_yaw, cos_yaw) = YAW_DEGREES.to_radians().sin_cos();
        camera.project(center_x + x * cos_yaw, center_y + y, center_z - x * sin_yaw)
    }
}

// Works for convex quads in either winding.
fn point_in_quad(point: Vector2, quad: &[Vector2; 4]) -> bool {
    let mut sign = 0.0f32;
    for i in 0..4 {
        let a = quad[i];
        let b = quad[(i + 1) % 4];
        let cross = (b.x - a.x) * (point.y - a.y) - (b.y - a.y) * (point.x - a.x);
        if cross.abs() < f32::EPSILON {
            continue;
        }
        if sign == 0.0 {
            sign = cross.signum();
        } else if cross.signum() != sign {
            return false;
        }
    }
    true
}
