use crate::blur_requester::BlurRequester;
use crate::overlay_manager::OverlayManager;
use crate::shaking::ShakeState;
use crate::window_manager::InputEvent;

use super::{AppOverlay, OVERLAY_FADE_DURATION};
/// Transient UI state: toggle animations, drags in progress, the
/// settings overlay's widget states, and the overlay and banner
/// machinery. Nothing here outlives the frame that produced it.
pub struct UiState {
    pub show_stats_animation: f32,
    pub vsync_animation: f32,
    pub tick_based_animation: f32,
    pub beat_bounce_animation: f32,
    pub beat_bounce_offbeat_animation: f32,
    pub shake_animation: f32,
    pub shake_state: ShakeState,
    pub keyboard_animation: f32,
    pub piano_roll_fullscreen_animation: f32,
    pub dock_controls_top_animation: f32,
    pub autoplay_animation: f32,
    pub pitch_bend_spring_sliders: [crate::ui_components::numeric_slider::NumericSliderState; 3],
    pub pitch_bend_move_window_animation: f32,
    pub pitch_bend_window_animation: f32,
    pub pitch_bend_override_animation: f32,
    pub unlimited_output_animation: f32,
    pub s16_clipping_animation: f32,
    /// When the S16 enable warning was opened, as program time in seconds
    /// from `get_time()`; `None` when closed.
    pub s16_warning_opened_at: Option<f64>,
    #[cfg(debug_assertions)]
    pub visqol_window_animation: f32,
    #[cfg(debug_assertions)]
    pub visqol_slider_dragging: bool,
    #[cfg(debug_assertions)]
    pub visqol_bitrate_field: crate::ui_components::numeric_slider::NumericSliderState,
    pub last_input_event: Option<InputEvent>,
    pub settings_scroll_y: f32,
    pub settings_scrollbar_dragging: bool,
    pub settings_scrollbar_drag_start_y: f32,
    pub settings_scrollbar_drag_start_scroll: f32,
    pub settings_pb_multiplier_slider: crate::ui_components::numeric_slider::NumericSliderState,
    pub settings_beat_bounce_amount_slider:
        crate::ui_components::numeric_slider::NumericSliderState,
    pub settings_beat_bounce_note_value_slider:
        crate::ui_components::numeric_slider::NumericSliderState,
    pub settings_midi_ev_buffer_size_slider:
        crate::ui_components::numeric_slider::NumericSliderState,
    pub settings_midi_ev_mode_combo: crate::ui_components::combo_box::ComboBoxState,
    pub settings_audio_engine_combo: crate::ui_components::combo_box::ComboBoxState,
    pub settings_audio_buffer_slider: crate::ui_components::numeric_slider::NumericSliderState,
    pub settings_opus_bitrate_slider: crate::ui_components::numeric_slider::NumericSliderState,
    pub engine_setting_sliders: std::collections::HashMap<String, crate::ui_components::numeric_slider::NumericSliderState>,
    pub settings_parse_threads_slider: crate::ui_components::numeric_slider::NumericSliderState,
    pub speed_slider_dragging: bool,
    pub text_editor: crate::ui_components::text_editor::TextEditorState,
    pub blur_requester: BlurRequester,
    pub overlay_manager: OverlayManager<AppOverlay>,
    /// Audio engine status banner: (message, is_error). Fades out after a while.
    pub audio_banner: Option<(String, bool)>,
    pub audio_banner_timer: f32,
}

impl UiState {
    pub fn new() -> Self {
        Self {
            show_stats_animation: 0.0,
            vsync_animation: 0.0,
            tick_based_animation: 0.0,
            beat_bounce_animation: 0.0,
            beat_bounce_offbeat_animation: 0.0,
            shake_animation: 0.0,
            shake_state: ShakeState::new(),
            keyboard_animation: 0.0,
            piano_roll_fullscreen_animation: 0.0,
            dock_controls_top_animation: 0.0,
            autoplay_animation: 0.0,
            pitch_bend_spring_sliders: std::array::from_fn(|_| Default::default()),
            pitch_bend_move_window_animation: 1.0,
            pitch_bend_window_animation: 1.0,
            pitch_bend_override_animation: 0.0,
            unlimited_output_animation: 0.0,
            s16_clipping_animation: 0.0,
            s16_warning_opened_at: None,
            #[cfg(debug_assertions)]
            visqol_window_animation: 0.0,
            #[cfg(debug_assertions)]
            visqol_slider_dragging: false,
            #[cfg(debug_assertions)]
            visqol_bitrate_field:
                crate::ui_components::numeric_slider::NumericSliderState::default(),
            last_input_event: None,
            settings_scroll_y: 0.0,
            settings_scrollbar_dragging: false,
            settings_scrollbar_drag_start_y: 0.0,
            settings_scrollbar_drag_start_scroll: 0.0,
            settings_pb_multiplier_slider: Default::default(),
            settings_beat_bounce_amount_slider:
                crate::ui_components::numeric_slider::NumericSliderState::default(),
            settings_beat_bounce_note_value_slider:
                crate::ui_components::numeric_slider::NumericSliderState::default(),
            settings_midi_ev_buffer_size_slider:
                crate::ui_components::numeric_slider::NumericSliderState::default(),
            settings_midi_ev_mode_combo: crate::ui_components::combo_box::ComboBoxState::default(),
            settings_audio_engine_combo: crate::ui_components::combo_box::ComboBoxState::default(),
            settings_audio_buffer_slider:
                crate::ui_components::numeric_slider::NumericSliderState::default(),
            settings_opus_bitrate_slider:
                crate::ui_components::numeric_slider::NumericSliderState::default(),
            engine_setting_sliders: Default::default(),
            settings_parse_threads_slider: Default::default(),
            speed_slider_dragging: false,
            text_editor: Default::default(),
            blur_requester: BlurRequester::new(),
            overlay_manager: OverlayManager::new(OVERLAY_FADE_DURATION),
            audio_banner: None,
            audio_banner_timer: 0.0,
        }
    }
}
