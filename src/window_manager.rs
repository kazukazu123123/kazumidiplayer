use std::collections::HashMap;
use std::sync::atomic::{AtomicI32, Ordering};

// Structure representing color
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub fn from_argb(a: u8, r: u8, g: u8, b: u8) -> Self {
        Color { r, g, b, a }
    }

    pub fn adjust_brightness(&self, factor: f64) -> Color {
        let r = (self.r as f64 * factor).clamp(0.0, 255.0) as u8;
        let g = (self.g as f64 * factor).clamp(0.0, 255.0) as u8;
        let b = (self.b as f64 * factor).clamp(0.0, 255.0) as u8;
        Color { r, g, b, a: self.a }
    }
}

// Structure representing a rectangle
#[derive(Debug, Clone, Copy)]
pub struct Rectangle {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Rectangle {
    pub fn new(x: i32, y: i32, width: i32, height: i32) -> Self {
        Rectangle {
            x,
            y,
            width,
            height,
        }
    }

    pub fn left(&self) -> i32 {
        self.x
    }
    pub fn top(&self) -> i32 {
        self.y
    }
    pub fn right(&self) -> i32 {
        self.x + self.width
    }
    pub fn bottom(&self) -> i32 {
        self.y + self.height
    }

    pub fn contains(&self, point: Point) -> bool {
        point.x >= self.x
            && point.x < self.x + self.width
            && point.y >= self.y
            && point.y < self.y + self.height
    }

    pub fn clamp_point(&self, point: Point) -> Point {
        if self.width <= 0 || self.height <= 0 {
            return point;
        }
        Point {
            x: point.x.clamp(self.x, self.x + self.width - 1),
            y: point.y.clamp(self.y, self.y + self.height - 1),
        }
    }
}

// Structure representing a point
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

// Resize mode enum
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ResizeMode {
    None,
    Left,
    Right,
    Top,
    Bottom,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

// Cursor type enum
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CursorType {
    Default,
    ResizeEW,   // East-West (horizontal)
    ResizeNS,   // North-South (vertical)
    ResizeNWSE, // Northwest-Southeast (diagonal)
    ResizeNESW, // Northeast-Southwest (diagonal)
    NotAllowed,
}

// Input event enum
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InputEvent {
    MouseDown(Point),
    MouseMove(Point),
    MouseUp(Point),
}

// Global ID and Z-order management
static LAST_ID: AtomicI32 = AtomicI32::new(0);
static LAST_Z_ORDER: AtomicI32 = AtomicI32::new(0);

const MIN_WIDTH: i32 = 100;
const MIN_HEIGHT: i32 = 40;
pub const BORDER_THICKNESS: i32 = 4;

// Type for draw callbacks
// Type for input callbacks

// Window structure
pub struct Window {
    pub id: i32,
    pub z_order: i32,
    pub title: String,
    pub is_active: bool,
    pub visible: bool,
    pub rectangle: Rectangle,
    window_titlebar_height: i32,
    pub min_width: Option<i32>,
    pub min_height: Option<i32>,
    pub max_width: Option<i32>,
    pub max_height: Option<i32>,
}

impl Window {
    pub fn new(
        title: String,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        window_titlebar_height: i32,
    ) -> Self {
        let id = LAST_ID.fetch_add(1, Ordering::SeqCst) + 1;
        let z_order = LAST_Z_ORDER.fetch_add(1, Ordering::SeqCst) + 1;

        Window {
            id,
            z_order,
            title,
            is_active: false,
            visible: true,
            rectangle: Rectangle::new(
                x.max(0),
                y.max(0),
                width.max(MIN_WIDTH),
                height.max(MIN_HEIGHT) + window_titlebar_height,
            ),
            window_titlebar_height,
            min_width: None,
            min_height: None,
            max_width: None,
            max_height: None,
        }
    }

    pub fn handle_input(&mut self, _event: InputEvent) {
    }

    pub fn get_window_titlebar_height(&self) -> i32 {
        self.window_titlebar_height
    }
}

// Window Manager
pub struct WindowManager {
    max_bounds: Rectangle,
    last_mouse_pos: Point,
    last_window_pos: Point,
    is_dragging: bool,
    dragging_window: Option<i32>,
    is_resizing: bool,
    resizing_window: Option<i32>,
    resize_mode: ResizeMode,
    resize_start_rect: Rectangle,

    theme_default_color: Color,
    base_color: Color,
    window_titlebar_active_color: Color,
    window_titlebar_inactive_color: Color,
    window_background_color: Color,
    window_border_color: Color,
    pub window_titlebar_height: i32,

    windows: HashMap<i32, Window>,
    z_order_to_id: HashMap<i32, i32>,
}

impl WindowManager {
    pub fn new() -> Self {
        let theme_default_color = Color::from_argb(255, 64, 128, 0);
        let calculated_titlebar_height = 24;

        let mut manager = WindowManager {
            max_bounds: Rectangle::new(0, 0, 0, 0),
            last_mouse_pos: Point { x: 0, y: 0 },
            last_window_pos: Point { x: 0, y: 0 },
            is_dragging: false,
            dragging_window: None,
            is_resizing: false,
            resizing_window: None,
            resize_mode: ResizeMode::None,
            resize_start_rect: Rectangle::new(0, 0, 0, 0),
            theme_default_color,
            base_color: theme_default_color, // Placeholder
            window_titlebar_active_color: theme_default_color, // Placeholder
            window_titlebar_inactive_color: theme_default_color, // Placeholder
            window_background_color: theme_default_color, // Placeholder
            window_border_color: theme_default_color, // Placeholder
            window_titlebar_height: calculated_titlebar_height,
            windows: HashMap::new(),
            z_order_to_id: HashMap::new(),
        };

        manager.set_theme_color(None);
        manager
    }

    pub fn set_theme_color(&mut self, base_color: Option<Color>) {
        let effective_base_color = base_color.unwrap_or(self.theme_default_color);

        self.base_color = effective_base_color;
        self.window_titlebar_active_color = effective_base_color;
        self.window_background_color = effective_base_color.adjust_brightness(1.2);
        self.window_titlebar_inactive_color = effective_base_color.adjust_brightness(0.7);
        self.window_border_color = effective_base_color.adjust_brightness(0.6);
    }

    /// Returns visible window IDs sorted by Z-order (ascending = back to front).
    pub fn get_window_ids_sorted(&self) -> Vec<i32> {
        let mut z_orders: Vec<i32> = self
            .windows
            .iter()
            .filter(|(_, w)| w.visible)
            .map(|(_, w)| w.z_order)
            .collect();
        z_orders.sort();
        z_orders
            .iter()
            .filter_map(|z| self.z_order_to_id.get(z))
            .copied()
            .collect()
    }

    pub fn get_window_border_color(&self) -> Color {
        self.window_border_color
    }

    pub fn get_window_background_color(&self) -> Color {
        self.window_background_color
    }

    pub fn get_window_titlebar_active_color(&self) -> Color {
        self.window_titlebar_active_color
    }

    pub fn get_window_titlebar_inactive_color(&self) -> Color {
        self.window_titlebar_inactive_color
    }

    pub fn new_window(&mut self, title: &str, x: i32, y: i32, width: i32, height: i32) -> i32 {
        let window = Window::new(
            title.to_string(),
            x,
            y,
            width,
            height,
            self.window_titlebar_height,
        );
        let window_id = window.id;
        let window_z_order = window.z_order;

        self.windows.insert(window_id, window);
        self.z_order_to_id.insert(window_z_order, window_id);

        self.activate_window(window_id);
        window_id
    }

    pub fn activate_window(&mut self, window_id: i32) {
        // Deactivate all windows
        self.deactivate_all_windows();

        if let Some(window) = self.windows.get_mut(&window_id) {
            // Set new Z-order
            let new_z_order = LAST_Z_ORDER.fetch_add(1, Ordering::SeqCst) + 1;
            window.z_order = new_z_order;
            window.is_active = true;

            // Update Z-order to ID mapping
            self.z_order_to_id.insert(new_z_order, window_id);
        }
    }
    pub fn deactivate_all_windows(&mut self) {
        for window in self.windows.values_mut() {
            window.is_active = false;
        }
    }

    pub fn mouse_down(&mut self, mouse_location: Point, bounds: Rectangle) -> Option<i32> {
        self.max_bounds = bounds;
        let mouse_location = self.max_bounds.clamp_point(mouse_location);

        let mut target_window_id = None;

        // Check windows in reverse Z-order (top to bottom)
        let mut z_orders: Vec<i32> = self.windows.values().map(|w| w.z_order).collect();
        z_orders.sort_by(|a, b| b.cmp(a));

        for z_order in z_orders {
            if let Some(&window_id) = self.z_order_to_id.get(&z_order) {
                if let Some(window) = self.windows.get(&window_id) {
                    if !window.visible {
                        continue;
                    }

                    let (resize_mode, _) =
                        self.get_resize_mode_at_position(window_id, mouse_location);

                    if resize_mode != ResizeMode::None {
                        let start_rect = window.rectangle;
                        target_window_id = Some(window_id);
                        self.is_resizing = true;
                        self.resizing_window = Some(window_id);
                        self.resize_mode = resize_mode;
                        self.resize_start_rect = start_rect;
                        self.last_mouse_pos = mouse_location;
                        break;
                    }

                    let titlebar_rect = Rectangle::new(
                        window.rectangle.left(),
                        window.rectangle.top(),
                        window.rectangle.width,
                        self.window_titlebar_height,
                    );

                    if titlebar_rect.contains(mouse_location) {
                        let window_left = window.rectangle.left();
                        let window_top = window.rectangle.top();

                        target_window_id = Some(window_id);
                        self.last_mouse_pos = mouse_location;
                        self.last_window_pos = Point {
                            x: window_left,
                            y: window_top,
                        };
                        self.is_dragging = true;
                        self.dragging_window = Some(window_id);
                        break;
                    }

                    let window_rect = Rectangle::new(
                        window.rectangle.left() - BORDER_THICKNESS,
                        window.rectangle.top() - BORDER_THICKNESS,
                        window.rectangle.width + BORDER_THICKNESS * 2,
                        window.rectangle.height + BORDER_THICKNESS * 2,
                    );

                    if window_rect.contains(mouse_location) {
                        target_window_id = Some(window_id);
                        break;
                    }
                }
            }
        }

        if let Some(id) = target_window_id {
            self.activate_window(id);
            // Re-borrow window to handle input safely
            if let Some(window) = self.windows.get_mut(&id) {
                window.handle_input(InputEvent::MouseDown(mouse_location));
            }
            return Some(id);
        }

        // If no window was clicked, deactivate all windows
        self.deactivate_all_windows();
        None
    }

    pub fn mouse_move(&mut self, mouse_location: Point) -> Option<i32> {
        let mouse_location = self.max_bounds.clamp_point(mouse_location);
        if self.is_resizing {
            if let Some(window_id) = self.resizing_window {
                if let Some(window) = self.windows.get_mut(&window_id) {
                    let delta_x = mouse_location.x - self.last_mouse_pos.x;
                    let delta_y = mouse_location.y - self.last_mouse_pos.y;

                    let mut new_x = self.resize_start_rect.x;
                    let mut new_y = self.resize_start_rect.y;
                    let mut new_width = self.resize_start_rect.width;
                    let mut new_height = self.resize_start_rect.height;

                    let min_w = window.min_width.unwrap_or(MIN_WIDTH);
                    let min_h =
                        window.min_height.unwrap_or(MIN_HEIGHT) + window.window_titlebar_height;
                    let max_w = window.max_width.unwrap_or(i32::MAX);
                    let max_h = window
                        .max_height
                        .unwrap_or(i32::MAX - window.window_titlebar_height)
                        + window.window_titlebar_height;

                    match self.resize_mode {
                        ResizeMode::Left => {
                            new_width =
                                (self.resize_start_rect.width - delta_x).clamp(min_w, max_w);
                            new_x = self.resize_start_rect.right() - new_width;
                        }
                        ResizeMode::Right => {
                            new_width =
                                (self.resize_start_rect.width + delta_x).clamp(min_w, max_w);
                        }
                        ResizeMode::Top => {
                            new_height =
                                (self.resize_start_rect.height - delta_y).clamp(min_h, max_h);
                            new_y = self.resize_start_rect.bottom() - new_height;
                        }
                        ResizeMode::Bottom => {
                            new_height =
                                (self.resize_start_rect.height + delta_y).clamp(min_h, max_h);
                        }
                        ResizeMode::TopLeft => {
                            new_width =
                                (self.resize_start_rect.width - delta_x).clamp(min_w, max_w);
                            new_x = self.resize_start_rect.right() - new_width;
                            new_height =
                                (self.resize_start_rect.height - delta_y).clamp(min_h, max_h);
                            new_y = self.resize_start_rect.bottom() - new_height;
                        }
                        ResizeMode::TopRight => {
                            new_width =
                                (self.resize_start_rect.width + delta_x).clamp(min_w, max_w);
                            new_height =
                                (self.resize_start_rect.height - delta_y).clamp(min_h, max_h);
                            new_y = self.resize_start_rect.bottom() - new_height;
                        }
                        ResizeMode::BottomLeft => {
                            new_width =
                                (self.resize_start_rect.width - delta_x).clamp(min_w, max_w);
                            new_x = self.resize_start_rect.right() - new_width;
                            new_height =
                                (self.resize_start_rect.height + delta_y).clamp(min_h, max_h);
                        }
                        ResizeMode::BottomRight => {
                            new_width =
                                (self.resize_start_rect.width + delta_x).clamp(min_w, max_w);
                            new_height =
                                (self.resize_start_rect.height + delta_y).clamp(min_h, max_h);
                        }
                        ResizeMode::None => {}
                    }

                    window.rectangle = Rectangle::new(new_x, new_y, new_width, new_height);
                    return Some(window_id);
                }
            }
        } else if self.is_dragging {
            if let Some(window_id) = self.dragging_window {
                if let Some(window) = self.windows.get_mut(&window_id) {
                    let delta_x = mouse_location.x - self.last_mouse_pos.x;
                    let delta_y = mouse_location.y - self.last_mouse_pos.y;

                    let new_x = self.last_window_pos.x + delta_x;
                    let new_y = self.last_window_pos.y + delta_y;

                    window.rectangle = Rectangle::new(
                        new_x,
                        new_y,
                        window.rectangle.width,
                        window.rectangle.height,
                    );
                    return Some(window_id);
                }
            }
        } else {
            // Forward mouse move to active window if it's over it
            let mut z_orders: Vec<i32> = self.windows.values().map(|w| w.z_order).collect();
            z_orders.sort_by(|a, b| b.cmp(a));

            for z_order in z_orders {
                if let Some(&window_id) = self.z_order_to_id.get(&z_order) {
                    if let Some(window) = self.windows.get_mut(&window_id) {
                        if !window.visible {
                            continue;
                        }
                        let window_rect = Rectangle::new(
                            window.rectangle.left() - BORDER_THICKNESS,
                            window.rectangle.top() - BORDER_THICKNESS,
                            window.rectangle.width + BORDER_THICKNESS * 2,
                            window.rectangle.height + BORDER_THICKNESS * 2,
                        );

                        if window_rect.contains(mouse_location) {
                            window.handle_input(InputEvent::MouseMove(mouse_location));
                            return Some(window_id);
                        }
                    }
                }
            }
        }
        None
    }

    pub fn mouse_up(&mut self, mouse_location: Point) -> Option<i32> {
        let mouse_location = self.max_bounds.clamp_point(mouse_location);
        let dragging_win = self.dragging_window;
        let resizing_win = self.resizing_window;
        let was_interacting = self.is_dragging || self.is_resizing;

        self.is_dragging = false;
        self.dragging_window = None;
        self.is_resizing = false;
        self.resizing_window = None;
        self.resize_mode = ResizeMode::None;

        if was_interacting {
            return dragging_win.or(resizing_win);
        }

        // Forward mouse up to window under cursor
        let mut z_orders: Vec<i32> = self.windows.values().map(|w| w.z_order).collect();
        z_orders.sort_by(|a, b| b.cmp(a));

        for z_order in z_orders {
            if let Some(&window_id) = self.z_order_to_id.get(&z_order) {
                if let Some(window) = self.windows.get_mut(&window_id) {
                    if !window.visible {
                        continue;
                    }
                    let window_rect = Rectangle::new(
                        window.rectangle.left() - BORDER_THICKNESS,
                        window.rectangle.top() - BORDER_THICKNESS,
                        window.rectangle.width + BORDER_THICKNESS * 2,
                        window.rectangle.height + BORDER_THICKNESS * 2,
                    );

                    if window_rect.contains(mouse_location) {
                        window.handle_input(InputEvent::MouseUp(mouse_location));
                        return Some(window_id);
                    }
                }
            }
        }
        None
    }

    pub fn get_window(&self, window_id: i32) -> Option<&Window> {
        self.windows.get(&window_id)
    }

    pub fn set_window_visible(&mut self, window_id: i32, visible: bool) {
        if let Some(window) = self.windows.get_mut(&window_id) {
            window.visible = visible;
            if !visible {
                window.is_active = false;
            }
        }
    }

    pub fn move_window(&mut self, window_id: i32, x: i32, y: i32) {
        if let Some(window) = self.windows.get_mut(&window_id) {
            window.rectangle.x = x;
            window.rectangle.y = y;
        }
    }

    pub fn set_window_min_size(
        &mut self,
        window_id: i32,
        min_width: Option<i32>,
        min_height: Option<i32>,
    ) {
        if let Some(window) = self.windows.get_mut(&window_id) {
            window.min_width = min_width;
            window.min_height = min_height;

            // Enforce the new minimum size immediately
            let min_w = min_width.unwrap_or(MIN_WIDTH);
            let min_h = min_height.unwrap_or(MIN_HEIGHT);

            let new_width = window.rectangle.width.max(min_w);
            let new_height = (window.rectangle.height - window.window_titlebar_height).max(min_h)
                + window.window_titlebar_height;

            if window.rectangle.width != new_width || window.rectangle.height != new_height {
                window.rectangle = Rectangle::new(
                    window.rectangle.x,
                    window.rectangle.y,
                    new_width,
                    new_height,
                );
            }
        }
    }

    pub fn set_window_max_size(
        &mut self,
        window_id: i32,
        max_width: Option<i32>,
        max_height: Option<i32>,
    ) {
        if let Some(window) = self.windows.get_mut(&window_id) {
            window.max_width = max_width;
            window.max_height = max_height;

            // Enforce the new maximum size immediately
            let max_w = max_width.unwrap_or(i32::MAX);
            let max_h = max_height.unwrap_or(i32::MAX - window.window_titlebar_height);

            let new_width = window.rectangle.width.min(max_w);
            let new_height = (window.rectangle.height - window.window_titlebar_height).min(max_h)
                + window.window_titlebar_height;

            if window.rectangle.width != new_width || window.rectangle.height != new_height {
                window.rectangle = Rectangle::new(
                    window.rectangle.x,
                    window.rectangle.y,
                    new_width,
                    new_height,
                );
            }
        }
    }

    pub fn get_resize_mode_at_position(
        &self,
        window_id: i32,
        mouse_pos: Point,
    ) -> (ResizeMode, CursorType) {
        if let Some(window) = self.windows.get(&window_id) {
            let rect = &window.rectangle;
            let resize_margin = BORDER_THICKNESS * 2;

            let in_horizontal_range = mouse_pos.y >= rect.top() - resize_margin
                && mouse_pos.y <= rect.bottom() + resize_margin;
            let in_vertical_range = mouse_pos.x >= rect.left() - resize_margin
                && mouse_pos.x <= rect.right() + resize_margin;

            let on_left = in_horizontal_range
                && mouse_pos.x >= rect.left() - resize_margin
                && mouse_pos.x < rect.left();
            let on_right = in_horizontal_range
                && mouse_pos.x > rect.right()
                && mouse_pos.x <= rect.right() + resize_margin;
            let on_top = in_vertical_range
                && mouse_pos.y >= rect.top() - resize_margin
                && mouse_pos.y < rect.top();
            let on_bottom = in_vertical_range
                && mouse_pos.y > rect.bottom()
                && mouse_pos.y <= rect.bottom() + resize_margin;

            let min_w = window.min_width.unwrap_or(MIN_WIDTH);
            let max_w = window.max_width.unwrap_or(i32::MAX);
            let horiz_fixed = min_w >= max_w;

            let min_h = window.min_height.unwrap_or(MIN_HEIGHT);
            let max_h = window
                .max_height
                .unwrap_or(i32::MAX - window.window_titlebar_height);
            let vert_fixed = min_h >= max_h;

            if (on_top || on_bottom) && (on_left || on_right) {
                if horiz_fixed && vert_fixed {
                    return (ResizeMode::None, CursorType::NotAllowed);
                }
                let mode = if on_top && on_left {
                    ResizeMode::TopLeft
                } else if on_top && on_right {
                    ResizeMode::TopRight
                } else if on_bottom && on_left {
                    ResizeMode::BottomLeft
                } else {
                    ResizeMode::BottomRight
                };
                let cursor = if on_top && on_left || on_bottom && on_right {
                    CursorType::ResizeNWSE
                } else {
                    CursorType::ResizeNESW
                };
                return (mode, cursor);
            } else if on_left || on_right {
                if horiz_fixed {
                    return (ResizeMode::None, CursorType::NotAllowed);
                }
                let mode = if on_left {
                    ResizeMode::Left
                } else {
                    ResizeMode::Right
                };
                return (mode, CursorType::ResizeEW);
            } else if on_top || on_bottom {
                if vert_fixed {
                    return (ResizeMode::None, CursorType::NotAllowed);
                }
                let mode = if on_top {
                    ResizeMode::Top
                } else {
                    ResizeMode::Bottom
                };
                return (mode, CursorType::ResizeNS);
            }
        }
        (ResizeMode::None, CursorType::Default)
    }

    pub fn get_cursor_type(&self, mouse_pos: Point) -> CursorType {
        let mut z_orders: Vec<i32> = self.windows.values().map(|w| w.z_order).collect();
        z_orders.sort_by(|a, b| b.cmp(a));

        for z_order in z_orders {
            if let Some(&window_id) = self.z_order_to_id.get(&z_order) {
                if let Some(window) = self.windows.get(&window_id) {
                    if !window.visible {
                        continue;
                    }
                    let (_mode, cursor) = self.get_resize_mode_at_position(window_id, mouse_pos);
                    if cursor != CursorType::Default {
                        return cursor;
                    }

                    let window_rect = Rectangle::new(
                        window.rectangle.left() - BORDER_THICKNESS,
                        window.rectangle.top() - BORDER_THICKNESS,
                        window.rectangle.width + BORDER_THICKNESS * 2,
                        window.rectangle.height + BORDER_THICKNESS * 2,
                    );

                    if window_rect.contains(mouse_pos) {
                        return CursorType::Default;
                    }
                }
            }
        }
        CursorType::Default
    }
}
