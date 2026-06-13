use raylib::prelude::*;

pub struct OverlayManager<K> {
    active_kind: Option<K>,
    pending_kind: Option<K>,
    fade_timer: f32, // 0.0 to 1.0
    is_closing: bool,
    fade_duration: f32,
}

impl<K: PartialEq + Copy> OverlayManager<K> {
    pub fn new(fade_duration: f32) -> Self {
        Self {
            active_kind: None,
            pending_kind: None,
            fade_timer: 0.0,
            is_closing: false,
            fade_duration,
        }
    }

    pub fn open(&mut self, kind: K) {
        if let Some(active) = self.active_kind {
            if active == kind {
                // 同じ種類が開いている、または開きようとしている場合は
                // 閉じようとしていたとしてもキャンセルして開く方向に転換
                self.is_closing = false;
                self.pending_kind = None;
                return;
            }

            // 別の種類が開いている場合、それを閉じるフェーズへ
            // すでに別の pending がある場合は上書き（最新の要求を優先）
            self.pending_kind = Some(kind);
            self.is_closing = true;
            return;
        }

        // 何も開いていなければ即座に開始
        self.active_kind = Some(kind);
        self.pending_kind = None;
        self.is_closing = false;
    }

    pub fn current_kind(&self) -> Option<K> {
        // 現在表示中のもの、または次に表示予定のものを返す
        self.pending_kind.or(self.active_kind)
    }

    pub fn close(&mut self) {
        if self.active_kind.is_some() {
            self.is_closing = true;
            self.pending_kind = None;
        }
    }

    pub fn is_active(&self) -> bool {
        self.active_kind.is_some() && !self.is_closing && self.pending_kind.is_none()
    }

    pub fn get_fade_t(&self) -> f32 {
        self.fade_timer
    }

    pub fn blocks_input(&self) -> bool {
        self.fade_timer > 0.0
    }

    pub fn update(&mut self, dt: f32) {
        if self.active_kind.is_some() {
            if self.is_closing {
                self.fade_timer = (self.fade_timer - dt / self.fade_duration).max(0.0);

                // フェードアウト完了
                if self.fade_timer <= 0.0 {
                    if let Some(next) = self.pending_kind {
                        // 待機していた種類に切り替えて、フェードイン開始
                        self.active_kind = Some(next);
                        self.pending_kind = None;
                        self.is_closing = false;
                    } else {
                        self.active_kind = None;
                        self.is_closing = false;
                    }
                }
            } else {
                self.fade_timer = (self.fade_timer + dt / self.fade_duration).min(1.0);
            }
        }
    }

    pub fn draw<F>(
        &self,
        d: &mut RaylibDrawHandle,
        screen_width: i32,
        screen_height: i32,
        draw_content: F,
    ) where
        F: FnOnce(&mut RaylibDrawHandle, f32, K),
    {
        if self.fade_timer <= 0.0 {
            return;
        }

        if let Some(kind) = self.active_kind {
            let overlay_alpha = (self.fade_timer * 180.0) as u8;
            d.draw_rectangle(
                0,
                0,
                screen_width,
                screen_height,
                Color::new(0, 0, 0, overlay_alpha),
            );
            draw_content(d, self.fade_timer, kind);
        }
    }
}
