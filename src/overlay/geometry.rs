//! Rectangles and the screen-size scale shared by the menu and the prompts.

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

impl Rect {
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.left && x < self.right && y >= self.top && y < self.bottom
    }

    pub fn width(&self) -> f32 {
        self.right - self.left
    }

    #[cfg(test)]
    pub fn height(&self) -> f32 {
        self.bottom - self.top
    }

    pub fn shifted(&self, dx: f32) -> Rect {
        Rect { left: self.left + dx, right: self.right + dx, ..*self }
    }

    /// The same rectangle shrunk by `margin` on every side.
    pub fn inset(&self, margin: f32) -> Rect {
        Rect {
            left: self.left + margin,
            top: self.top + margin,
            right: self.right - margin,
            bottom: self.bottom - margin,
        }
    }
}

/// One design unit in pixels: 1.0 on a 720-pixel-high screen, growing with
/// the screen so text and buttons keep their physical size on the handheld,
/// and shrinking a little so everything still fits in a smaller window.
pub fn unit(height: f32) -> f32 {
    (height / 720.0).clamp(0.7, 1.75)
}
