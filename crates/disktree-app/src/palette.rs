//! Colour that means something.
//!
//! A tile's hue says what kind of data it is ([`Category`]); every hue sits
//! at the same muted saturation and lightness, so no block stands out by
//! accident, and deeper tiles lift slightly so nesting reads without borders.
//! Reclaimable space is a hatch, not a colour, so "what is it" and "can it go"
//! are read independently.
//!
//! One strong colour is kept apart: the highlight, the theme's warning amber.
//! It marks the selection, the main action, reclaimable totals and the free
//! space after a removal, and nothing else, so the eye goes straight to it.
//!
//! Lightness, saturation and the surface every fill is pulled toward come
//! from the active Omarchy theme, so the mosaic sits inside it, light or dark.

use disktree_core::classify::Category;
use gpui_kit::base::ThemeAppearance;
use gpui_kit::{Hsla, Rgba};
use gpui_omarchy::Theme;

/// The hue a category is drawn in, and how much colour it carries. The
/// neutral kinds (documents, unknown) carry almost none.
const fn hue(category: Category) -> (f32, f32) {
    match category {
        Category::Code => (0.605, 1.0),
        Category::AgentScratch => (0.065, 1.0),
        Category::Toolchain => (0.415, 1.0),
        Category::Synced => (0.535, 1.0),
        Category::Git => (0.955, 1.0),
        Category::Media => (0.745, 1.0),
        Category::Cache => (0.125, 0.95),
        Category::FilesystemEvents => (0.31, 0.6),
        Category::Spotlight => (0.855, 0.6),
        Category::Documents => (0.6, 0.18),
        Category::Other => (0.6, 0.08),
    }
}

const fn dark(theme: &Theme) -> bool {
    matches!(theme.appearance, ThemeAppearance::Dark)
}

/// The fill for a tile of `category`, `depth` levels into the view.
pub fn category_fill(theme: &Theme, category: Category, depth: u32) -> Hsla {
    let (h, chroma) = hue(category);
    let step = depth.min(4) as f32;
    let (s, l) = if dark(theme) {
        (0.26 * chroma, step.mul_add(0.028, 0.215))
    } else {
        (0.30 * chroma, step.mul_add(-0.03, 0.84))
    };
    // Pulled a little toward the theme surface, so each theme tints it.
    mix(Hsla { h, s, l, a: 1.0 }, theme.inset, 0.12)
}

/// The saturated version of a category's hue: the strip over a top-level
/// directory and the legend swatch.
pub fn category_accent(theme: &Theme, category: Category) -> Hsla {
    let (h, chroma) = hue(category);
    let (s, l) = if dark(theme) {
        (0.42 * chroma, 0.52)
    } else {
        (0.45 * chroma, 0.46)
    };
    Hsla { h, s, l, a: 1.0 }
}

/// The age ramp, newest first: this week, this month, this half-year, this
/// year, older.
pub const AGE_BUCKETS: [(i64, &str); 5] = [
    (7, "This week"),
    (30, "This month"),
    (182, "Six months"),
    (365, "This year"),
    (i64::MAX, "Older"),
];

/// Which [`AGE_BUCKETS`] entry an age in days falls in.
pub fn age_bucket(days: i64) -> usize {
    AGE_BUCKETS
        .iter()
        .position(|(limit, _)| days <= *limit)
        .unwrap_or(AGE_BUCKETS.len() - 1)
}

/// The fill for age mode: recent writes carry the theme accent, and colour
/// drains out of a tile as it goes untouched.
pub fn age_fill(theme: &Theme, bucket: usize, depth: u32) -> Hsla {
    let fade = bucket.min(AGE_BUCKETS.len() - 1) as f32 / 4.0;
    let step = depth.min(4) as f32;
    let (s, l) = if dark(theme) {
        (
            (1.0 - fade).mul_add(0.34, 0.03),
            step.mul_add(0.028, 0.29 - fade * 0.09),
        )
    } else {
        (
            (1.0 - fade).mul_add(0.36, 0.04),
            step.mul_add(-0.03, 0.74 + fade * 0.1),
        )
    };
    mix(
        Hsla {
            h: theme.accent.h,
            s,
            l,
            a: 1.0,
        },
        theme.inset,
        0.1,
    )
}

/// The age swatch for the legend.
pub fn age_accent(theme: &Theme, bucket: usize) -> Hsla {
    let fade = bucket.min(AGE_BUCKETS.len() - 1) as f32 / 4.0;
    let l = if dark(theme) {
        0.55 - fade * 0.25
    } else {
        0.45 + fade * 0.25
    };
    Hsla {
        h: theme.accent.h,
        s: (1.0 - fade).mul_add(0.45, 0.04),
        l,
        a: 1.0,
    }
}

/// The one strong colour: selection, the main action, what can be had back.
pub const fn highlight(theme: &Theme) -> Hsla {
    theme.warning
}

/// Text on a filled highlight.
pub const fn on_highlight(theme: &Theme) -> Hsla {
    if dark(theme) {
        theme.background
    } else {
        theme.bright
    }
}

/// The diagonal hatch over reclaimable space: quiet enough to leave the hue
/// readable, visible on every fill.
pub fn hatch(theme: &Theme) -> Hsla {
    if dark(theme) {
        theme.bright.opacity(0.16)
    } else {
        theme.foreground.opacity(0.18)
    }
}

/// Linear interpolation between two colours, in RGB: interpolating hue
/// would drag a colour around the wheel on its way to a grey.
pub fn mix(from: Hsla, to: Hsla, t: f32) -> Hsla {
    let t = t.clamp(0.0, 1.0);
    let lerp = |a: f32, b: f32| (b - a).mul_add(t, a);
    let (a, b) = (from.to_rgb(), to.to_rgb());
    Hsla::from(Rgba {
        r: lerp(a.r, b.r),
        g: lerp(a.g, b.g),
        b: lerp(a.b, b.b),
        a: lerp(a.a, b.a),
    })
}

/// A tile's name on top of its fill.
pub fn label_color(theme: &Theme, depth: u32) -> Hsla {
    let base = if dark(theme) {
        theme.bright
    } else {
        theme.foreground
    };
    if depth == 0 { base } else { base.opacity(0.88) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme(appearance: ThemeAppearance) -> Theme {
        match appearance {
            ThemeAppearance::Dark => Theme::tokyo_night(),
            ThemeAppearance::Light => Theme::flexoki_light(),
        }
    }

    #[test]
    fn every_legend_category_has_its_own_hue() {
        let theme = theme(ThemeAppearance::Dark);
        let fills: Vec<Hsla> = Category::LEGEND
            .iter()
            .map(|&category| category_accent(&theme, category))
            .collect();
        for (index, left) in fills.iter().enumerate() {
            for right in &fills[index + 1..] {
                let apart = (left.h - right.h).abs() > 0.03
                    || (left.s - right.s).abs() > 0.1;
                assert!(apart, "{left:?} and {right:?} read as one colour");
            }
        }
    }

    #[test]
    fn colourful_categories_share_one_level() {
        let theme = theme(ThemeAppearance::Dark);
        let code = category_fill(&theme, Category::Code, 0);
        let git = category_fill(&theme, Category::Git, 0);
        assert!((code.l - git.l).abs() < 0.02);
        assert!((code.s - git.s).abs() < 0.03);
    }

    #[test]
    fn deeper_tiles_lift_away_from_the_background() {
        for appearance in [ThemeAppearance::Dark, ThemeAppearance::Light] {
            let theme = theme(appearance);
            let top = category_fill(&theme, Category::Code, 0);
            let deep = category_fill(&theme, Category::Code, 3);
            // Away from the background: lighter on dark, darker on light.
            let distance = |fill: Hsla| (fill.l - theme.background.l).abs();
            assert!(distance(deep) > distance(top) + 0.05, "{appearance:?}");
        }
    }

    #[test]
    fn the_highlight_is_not_a_category_colour() {
        let theme = theme(ThemeAppearance::Dark);
        let highlight = highlight(&theme);
        for category in Category::LEGEND {
            let fill = category_fill(&theme, category, 0);
            assert!(highlight.s - fill.s > 0.2, "{category:?} competes");
        }
    }

    #[test]
    fn age_buckets_cover_every_age_in_order() {
        assert_eq!(age_bucket(0), 0);
        assert_eq!(age_bucket(8), 1);
        assert_eq!(age_bucket(100), 2);
        assert_eq!(age_bucket(300), 3);
        assert_eq!(age_bucket(5000), 4);
        let theme = theme(ThemeAppearance::Dark);
        assert!(age_fill(&theme, 0, 0).s > age_fill(&theme, 4, 0).s);
    }

    #[test]
    fn mixing_toward_a_grey_keeps_the_hue() {
        let theme = theme(ThemeAppearance::Dark);
        let orange = category_accent(&theme, Category::AgentScratch);
        let mixed = mix(orange, theme.inset, 0.3);
        assert!((mixed.h - orange.h).abs() < 0.02, "{mixed:?}");
    }

    #[test]
    fn mix_clamps_its_parameter() {
        let theme = theme(ThemeAppearance::Dark);
        let clamped_low = mix(theme.background, theme.accent, -1.0).l;
        let clamped_high = mix(theme.background, theme.accent, 2.0).l;
        assert!((clamped_low - theme.background.l).abs() < 1e-3);
        assert!((clamped_high - theme.accent.l).abs() < 1e-3);
    }
}
