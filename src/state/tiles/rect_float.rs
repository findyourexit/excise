use ratatui::layout::Rect;

#[derive(Clone, Debug)]
pub struct RectFloat {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl RectFloat {
    pub fn new(rect: Rect) -> Self {
        Self {
            x: f64::from(rect.x),
            y: f64::from(rect.y),
            height: f64::from(rect.height),
            width: f64::from(rect.width),
        }
    }
    pub fn round(&self) -> Rect {
        fn coordinate(value: f64) -> u16 {
            if value.is_finite() {
                value.round().clamp(0.0, f64::from(u16::MAX)) as u16
            } else {
                0
            }
        }

        let x = coordinate(self.x);
        let y = coordinate(self.y);
        let right = coordinate(self.x + self.width.max(0.0)).max(x);
        let bottom = coordinate(self.y + self.height.max(0.0)).max(y);
        Rect {
            x,
            y,
            width: right.saturating_sub(x),
            height: bottom.saturating_sub(y),
        }
    }
}
