//! Text forms of a color for the color picker.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum ColorFormat {
    Hex,
    Rgb,
    Hsl,
}

fn to_hsl(r: u8, g: u8, b: u8) -> (u32, u32, u32) {
    let (r, g, b) = (r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;
    let l = (max + min) / 2.0;
    if delta == 0.0 {
        return (0, 0, (l * 100.0).round() as u32);
    }
    let s = delta / (1.0 - (2.0 * l - 1.0).abs());
    let h = if max == r {
        60.0 * ((g - b) / delta).rem_euclid(6.0)
    } else if max == g {
        60.0 * ((b - r) / delta + 2.0)
    } else {
        60.0 * ((r - g) / delta + 4.0)
    };
    (h.round() as u32 % 360, (s * 100.0).round() as u32, (l * 100.0).round() as u32)
}

pub fn format_color(r: u8, g: u8, b: u8, format: ColorFormat) -> String {
    match format {
        ColorFormat::Hex => format!("#{r:02X}{g:02X}{b:02X}"),
        ColorFormat::Rgb => format!("rgb({r}, {g}, {b})"),
        ColorFormat::Hsl => {
            let (h, s, l) = to_hsl(r, g, b);
            format!("hsl({h}, {s}%, {l}%)")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_and_rgb_keep_the_channels_in_order() {
        assert_eq!(format_color(232, 18, 36, ColorFormat::Hex), "#E81224");
        assert_eq!(format_color(0, 10, 255, ColorFormat::Hex), "#000AFF");
        assert_eq!(format_color(232, 18, 36, ColorFormat::Rgb), "rgb(232, 18, 36)");
    }

    #[test]
    fn hsl_of_well_known_colors() {
        let hsl = |r, g, b| format_color(r, g, b, ColorFormat::Hsl);
        assert_eq!(hsl(255, 0, 0), "hsl(0, 100%, 50%)");
        assert_eq!(hsl(0, 255, 0), "hsl(120, 100%, 50%)");
        assert_eq!(hsl(0, 0, 255), "hsl(240, 100%, 50%)");
        assert_eq!(hsl(255, 255, 255), "hsl(0, 0%, 100%)");
        assert_eq!(hsl(0, 0, 0), "hsl(0, 0%, 0%)");
        assert_eq!(hsl(128, 128, 128), "hsl(0, 0%, 50%)");
        // Red with some blue is just below a full turn, not a negative angle.
        assert_eq!(hsl(255, 0, 128), "hsl(330, 100%, 50%)");
    }
}
