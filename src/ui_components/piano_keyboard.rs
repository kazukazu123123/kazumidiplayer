use raylib::prelude::*;

pub fn draw_piano_keyboard_layout_to_image(img: &mut Image, width: f32, height: f32) {
    img.clear_background(Color::new(0, 0, 0, 0));

    let get_color = |note: u8, is_black: bool, local_x: f32| -> Color {
        let r = note.saturating_mul(2);
        let g = if is_black { 128 } else { 255 };
        let b = (local_x * 255.0).clamp(0.0, 255.0) as u8;
        Color::new(r, g, b, 255)
    };

    let num_white_keys = 75;
    let black_key_width_ratio = 0.65;
    let black_key_height_ratio = 0.65;
    let layout_border_color = Color::new(0, 0, 0, 0);

    let w_total = width as i32;
    let h_total = height as i32;

    let get_white_key_rect = |idx: usize| {
        let x1 = (idx as f32 * width / num_white_keys as f32).round() as i32;
        let x2 = ((idx + 1) as f32 * width / num_white_keys as f32).round() as i32;
        let rw = (x2 - x1).min(w_total - x1);
        let rh = h_total;
        (x1, 0, rw, rh)
    };

    let mut white_key_idx = 0;
    let mut white_key_x_positions = [0i32; 128];

    for note in 0..128u8 {
        if !is_black_key(note) {
            let (kx, ky, kw, kh) = get_white_key_rect(white_key_idx);
            white_key_x_positions[note as usize] = kx;

            for dx in 0..kw {
                let local_x = dx as f32 / (kw - 1).max(1) as f32;
                let color = get_color(note, false, local_x);
                img.draw_rectangle(kx + dx, ky, 1, kh, color);
            }

            img.draw_rectangle(kx, ky, kw, 1, layout_border_color);
            img.draw_rectangle(kx, ky + kh - 1, kw, 1, layout_border_color);
            // Draw only the right vertical border so adjacent white keys share a
            // single 1px separator instead of creating a 2px transparent gap.
            img.draw_rectangle(kx + kw - 1, ky, 1, kh, layout_border_color);
            if white_key_idx == 0 {
                img.draw_rectangle(kx, ky, 1, kh, layout_border_color);
            }

            white_key_idx += 1;
        }
    }

    let white_key_width = width / num_white_keys as f32;
    let black_key_width_f = white_key_width * black_key_width_ratio;
    let black_key_height_f = height * black_key_height_ratio;

    white_key_idx = 0;
    for note in 0..128u8 {
        if is_black_key(note) {
            let next_white_x = {
                let mut search_idx = note + 1;
                while search_idx < 128 && is_black_key(search_idx) {
                    search_idx += 1;
                }
                if search_idx < 128 {
                    white_key_x_positions[search_idx as usize]
                } else {
                    (white_key_idx as f32 * white_key_width).round() as i32
                }
            };

            let mut shift = black_key_width_f / 2.0;
            let n = note % 12;
            if n == 1 || n == 6 {
                shift += black_key_width_f / 8.0;
            } else if n == 3 || n == 10 {
                shift -= black_key_width_f / 8.0;
            }

            let bx = (next_white_x as f32 - shift).round() as i32;
            let draw_w = (black_key_width_f.round() as i32).min(w_total - bx);
            let draw_h = (black_key_height_f.round() as i32).min(h_total);

            for dx in 0..draw_w {
                let local_x = dx as f32 / (draw_w - 1).max(1) as f32;
                let color = get_color(note, true, local_x);
                img.draw_rectangle(bx + dx, 0, 1, draw_h, color);
            }

            img.draw_rectangle(bx, 0, draw_w, 1, layout_border_color);
            img.draw_rectangle(bx, 0 + draw_h - 1, draw_w, 1, layout_border_color);
            img.draw_rectangle(bx, 0, 1, draw_h, layout_border_color);
            img.draw_rectangle(bx + draw_w - 1, 0, 1, draw_h, layout_border_color);
        } else {
            white_key_idx += 1;
        }
    }
}

pub fn get_keyboard_note_regions(width: f32) -> [(f32, f32); 128] {
    let mut regions = [(0.0, 0.0); 128];
    let num_white_keys = 75;
    let white_key_width = width / num_white_keys as f32;
    let black_key_width_ratio = 0.65;
    let black_key_width = white_key_width * black_key_width_ratio;

    let mut white_key_idx = 0;
    let mut white_key_x_positions = [0.0; 128];

    for note in 0..128u8 {
        if !is_black_key(note) {
            let x1 = (white_key_idx as f32 * width / num_white_keys as f32).round();
            let x2 = ((white_key_idx + 1) as f32 * width / num_white_keys as f32).round();
            let w = (x2 - x1).min(width - x1);
            regions[note as usize] = (x1, w);
            white_key_x_positions[note as usize] = x1;
            white_key_idx += 1;
        }
    }

    white_key_idx = 0;
    for note in 0..128u8 {
        if is_black_key(note) {
            let next_white_x = {
                let mut search_idx = note + 1;
                while search_idx < 128 && is_black_key(search_idx) {
                    search_idx += 1;
                }
                if search_idx < 128 {
                    white_key_x_positions[search_idx as usize]
                } else {
                    (white_key_idx as f32 * white_key_width).round()
                }
            };

            let mut shift = black_key_width / 2.0;
            let n = note % 12;
            if n == 1 || n == 6 {
                shift += black_key_width / 8.0;
            } else if n == 3 || n == 10 {
                shift -= black_key_width / 8.0;
            }

            let bx = (next_white_x - shift).round();
            let bw = black_key_width.min(width - bx);
            regions[note as usize] = (bx, bw);
        } else {
            white_key_idx += 1;
        }
    }
    regions
}

pub fn is_black_key(note: u8) -> bool {
    let n = note % 12;
    n == 1 || n == 3 || n == 6 || n == 8 || n == 10
}
