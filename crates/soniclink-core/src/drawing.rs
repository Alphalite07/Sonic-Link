use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct StrokePoint {
    pub x: u8,         // 0..255 normalized canvas X
    pub y: u8,         // 0..255 normalized canvas Y
    pub color_idx: u8, // 0..15 palette index
    pub pen_down: bool,
}

impl StrokePoint {
    /// Packs a stroke point into just 3 bytes for ultra-fast acoustic streaming!
    pub fn pack(&self) -> [u8; 3] {
        let meta = (self.color_idx & 0x0F) | if self.pen_down { 0x80 } else { 0x00 };
        [self.x, self.y, meta]
    }

    pub fn unpack(bytes: [u8; 3]) -> Self {
        Self {
            x: bytes[0],
            y: bytes[1],
            color_idx: bytes[2] & 0x0F,
            pen_down: (bytes[2] & 0x80) != 0,
        }
    }
}