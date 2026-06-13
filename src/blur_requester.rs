use raylib::prelude::*;
use std::collections::VecDeque;

#[derive(Debug, Clone)]
pub struct BlurRequest {
    pub blur_region: Rectangle,
    pub blur_strength: u16,
}

pub struct BlurRequester {
    pub requests: VecDeque<BlurRequest>,
}

impl BlurRequester {
    pub fn new() -> Self {
        Self {
            requests: VecDeque::new(),
        }
    }

    pub fn apply_blur(&mut self, blur_region: Rectangle, blur_strength: Option<u16>) {
        let blur_strength = blur_strength.unwrap_or(5);

        self.requests.push_back(BlurRequest {
            blur_region,
            blur_strength,
        });
    }

    pub fn consume_all(&mut self) -> Vec<BlurRequest> {
        self.requests.drain(..).collect()
    }
}
