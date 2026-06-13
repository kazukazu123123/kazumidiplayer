use raylib::prelude::*;

use crate::note_texture_generator;

/// Manages note textures and their lifecycle
pub struct NoteTextureManager {
    textures: Vec<(Texture2D, u64, u64)>,
    last_chunks_hash: u64,
}

impl NoteTextureManager {
    pub fn new() -> Self {
        Self {
            textures: Vec::new(),
            last_chunks_hash: 0,
        }
    }

    pub fn clear(&mut self) {
        self.textures.clear();
        self.last_chunks_hash = 0;
    }

    pub fn len(&self) -> usize {
        self.textures.len()
    }

    pub fn as_slice(&self) -> &[(Texture2D, u64, u64)] {
        &self.textures
    }

    /// Add a single texture from a chunk
    pub fn upload_texture(
        &mut self,
        chunk: &mut note_texture_generator::NoteChunkTexture,
        rl: &mut RaylibHandle,
        thread: &RaylibThread,
    ) {
        let rgba_data = match chunk.rgba_data.take() {
            Some(d) => d,
            None => return,
        };
        if rgba_data.is_empty() {
            return;
        }

        // Remove stale texture with same start time (e.g. after height change)
        self.textures
            .retain(|(_, start, _)| *start != chunk.start_time_ns);

        let img = Image::gen_image_color(chunk.width as i32, chunk.height as i32, Color::BLANK);
        let pixel_count = chunk.width * chunk.height;
        let expected_bytes = pixel_count * 4;
        let copy_len = expected_bytes.min(rgba_data.len());
        // Safety: img.data is a valid pointer from raylib's GenImageColor.
        // expected_bytes matches the allocation size of the image.
        unsafe {
            let img_data = std::slice::from_raw_parts_mut(img.data as *mut u8, expected_bytes);
            img_data[..copy_len].copy_from_slice(&rgba_data[..copy_len]);
        }

        if let Ok(tex) = rl.load_texture_from_image(thread, &img) {
            tex.set_texture_filter(thread, TextureFilter::TEXTURE_FILTER_POINT);
            self.textures
                .push((tex, chunk.start_time_ns, chunk.end_time_ns));
            // Keep it sorted
            self.textures.sort_by_key(|(_, start, _)| *start);
        }
    }
}
