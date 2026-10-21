use crate::player::{PlayerHandle, PlayerState};
use crate::window_manager::WindowManager;

pub struct ShakeState {
    pub window_x: f32,
    pub window_y: f32,
    pub vel_x: f32,
    pub vel_y: f32,
}

impl ShakeState {
    pub fn new() -> Self {
        Self {
            window_x: f32::NAN,
            window_y: f32::NAN,
            vel_x: 0.0,
            vel_y: 0.0,
        }
    }

    pub fn update(
        &mut self,
        enabled: bool,
        integrate_keyboard: bool,
        piano_roll_handle: i32,
        piano_keyboard_handle: i32,
        window_manager: &mut WindowManager,
        screen_width: i32,
        screen_height: i32,
        engine: &PlayerHandle,
        is_midi_loaded: bool,
        is_loading: bool,
        player_state: PlayerState,
        is_dragging_seek: bool,
        dt: f32,
    ) {
        if !enabled {
            self.vel_x = 0.0;
            self.vel_y = 0.0;
            self.window_x = f32::NAN;
            self.window_y = f32::NAN;
            return;
        }

        let shaking_target_handle = if integrate_keyboard {
            piano_roll_handle
        } else {
            piano_keyboard_handle
        };

        let sh_win = match window_manager.get_window(shaking_target_handle) {
            Some(w) => w,
            None => return,
        };

        let win_w = sh_win.rectangle.width as f32;
        let win_h = sh_win.rectangle.height as f32;
        let screen_center_x = screen_width as f32 / 2.0;
        let screen_center_y = screen_height as f32 / 2.0;
        let target_x = screen_center_x - win_w / 2.0;
        let target_y = screen_center_y - win_h / 2.0;
        let tension: f32 = 0.08;
        let damping: f32 = 0.95;

        let dt_scale = dt * 60.0;

        if self.window_x.is_nan() {
            self.window_x = sh_win.rectangle.x as f32;
        }
        if self.window_y.is_nan() {
            self.window_y = sh_win.rectangle.y as f32;
        }

        self.vel_x += (target_x - self.window_x) * tension * dt_scale;
        self.vel_x *= damping.powf(dt_scale);

        self.vel_y += (target_y - self.window_y) * tension * dt_scale;
        self.vel_y *= damping.powf(dt_scale);

        let kp = 0.24;
        let kd = 0.18;
        let error = target_y - self.window_y;
        let derivative = -self.vel_y;
        let mut force_y = (kp * error + kd * derivative) * dt_scale;

        if is_midi_loaded
            && !is_loading
            && player_state == PlayerState::Playing
            && !is_dragging_seek
        {
            let active_notes = engine.get_active_notes();
            let mut left_count = 0;
            let mut right_count = 0;
            let mut keys_with_notes = 0;

            for i in 0..128 {
                if active_notes[i].is_some() {
                    keys_with_notes += 1;
                    if i < 64 {
                        left_count += 1;
                    } else {
                        right_count += 1;
                    }
                }
            }

            let total = left_count + right_count;
            if total > 0 {
                let ratio = (right_count as f32 - left_count as f32) / total as f32;
                let velocity_strength = 8.0;
                self.vel_x += ratio * velocity_strength * dt_scale;
            }

            force_y += (keys_with_notes as f32 / 128.0) * 16.0 * dt_scale;
        }

        self.vel_y += force_y;

        self.window_x += self.vel_x * dt_scale;
        self.window_y += self.vel_y * dt_scale;

        if !self.window_x.is_finite() || !self.window_y.is_finite() {
            self.window_x = target_x;
            self.window_y = target_y;
            self.vel_x = 0.0;
            self.vel_y = 0.0;
        }

        window_manager.move_window(
            shaking_target_handle,
            self.window_x as i32,
            self.window_y as i32,
        );
    }
}
