//! The tray icon is the chart: one small vertical bar per provider, filled to
//! how much of that provider's tightest quota is left.

/// Below this fraction left a quota is "getting low" (amber), and below
/// `LOW_BELOW` it is "nearly out" (red). The popover receives these same
/// numbers with every snapshot, so the icon and the bars always agree.
pub const WARN_BELOW: f64 = 0.5;
pub const LOW_BELOW: f64 = 0.2;

// Same hues as the popover's dark theme (--brand, --warn, --low in
// ui/style.css). A tray's background is unknown, so the lighter set is used.
// Purple is "plenty left"; amber and red only appear as a quota runs down.
const FULL: [u8; 4] = [0xd2, 0x8c, 0xe1, 255];
const WARN: [u8; 4] = [0xee, 0xbb, 0x58, 255];
const LOW: [u8; 4] = [0xf6, 0x6e, 0x60, 255];

pub const HEIGHT: u32 = 44; // 22pt at 2x, the macOS menubar size
const BAR: u32 = 8;
const GAP: u32 = 4;
const PAD: u32 = 3;
const MAX_BARS: usize = 6;

/// `levels`: remaining fraction per provider, `None` when unknown.
/// `template`: draw in black + alpha only, for macOS to tint to the menubar.
/// Returns RGBA pixels with their width and height.
pub fn render(levels: &[Option<f64>], template: bool) -> (Vec<u8>, u32, u32) {
    let placeholder = [None, None, None];
    let levels = if levels.is_empty() { &placeholder[..] } else { levels };
    let levels = &tightest(levels);

    let n = levels.len() as u32;
    let width = PAD * 2 + n * BAR + (n - 1) * GAP;
    let mut rgba = vec![0u8; (width * HEIGHT * 4) as usize];
    let (top, bottom) = (PAD, HEIGHT - PAD);
    let span = (bottom - top) as f64;

    for (i, level) in levels.iter().enumerate() {
        let x0 = PAD + i as u32 * (BAR + GAP);
        // Never draw a known-but-tiny quota as nothing at all.
        let fill_top = match level {
            Some(l) => bottom - ((span * l.clamp(0.0, 1.0)).round() as u32).max(2),
            None => bottom,
        };
        let fill = match (template, level) {
            (true, _) => [0, 0, 0, 255],
            (false, Some(l)) if *l < LOW_BELOW => LOW,
            (false, Some(l)) if *l < WARN_BELOW => WARN,
            (false, _) => FULL,
        };
        let track = if template { [0, 0, 0, 70] } else { [140, 140, 140, 110] };

        for y in top..bottom {
            for x in x0..x0 + BAR {
                // Knock the four corner pixels off each bar to round it.
                let edge_x = x == x0 || x == x0 + BAR - 1;
                let edge_y = y == top || y == bottom - 1;
                if edge_x && edge_y {
                    continue;
                }
                let colour = if y >= fill_top { fill } else { track };
                let at = ((y * width + x) * 4) as usize;
                rgba[at..at + 4].copy_from_slice(&colour);
            }
        }
    }
    (rgba, width, HEIGHT)
}

/// At most `MAX_BARS` levels, in their original order. When there are more,
/// the ones with the least left are kept (unknown counts as plenty), so a
/// provider that is running out never drops off the icon.
fn tightest(levels: &[Option<f64>]) -> Vec<Option<f64>> {
    if levels.len() <= MAX_BARS {
        return levels.to_vec();
    }
    let mut order: Vec<usize> = (0..levels.len()).collect();
    order.sort_by(|&a, &b| {
        let key = |i: usize| levels[i].unwrap_or(f64::INFINITY);
        key(a).total_cmp(&key(b))
    });
    order.truncate(MAX_BARS);
    order.sort_unstable();
    order.into_iter().map(|i| levels[i]).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filled_rows(rgba: &[u8], width: u32, x: u32) -> u32 {
        (0..HEIGHT)
            .filter(|y| rgba[((y * width + x) * 4 + 3) as usize] == 255)
            .count() as u32
    }

    #[test]
    fn bars_track_remaining_quota() {
        let (rgba, width, height) = render(&[Some(1.0), Some(0.5), Some(0.0), None], true);
        assert_eq!((width, height), (PAD * 2 + 4 * BAR + 3 * GAP, HEIGHT));
        assert_eq!(rgba.len() as u32, width * height * 4);
        let column = |i: u32| filled_rows(&rgba, width, PAD + i * (BAR + GAP) + BAR / 2);
        assert_eq!(column(0), HEIGHT - 2 * PAD); // full
        assert_eq!(column(1), (HEIGHT - 2 * PAD) / 2); // half
        assert_eq!(column(2), 2); // empty but known: a sliver
        assert_eq!(column(3), 0); // unknown: track only
    }

    #[test]
    fn too_many_providers_keep_the_tightest_bars() {
        let mut levels = vec![Some(0.9); MAX_BARS];
        levels.insert(2, None);
        levels.push(Some(0.05));
        let kept = tightest(&levels);
        assert_eq!(kept.len(), MAX_BARS);
        assert_eq!(kept.last(), Some(&Some(0.05)), "the nearly-empty one is drawn");
        assert!(!kept.contains(&None), "an unknown one gives way first");
        let (_, width, _) = render(&levels, true);
        assert_eq!(width, PAD * 2 + MAX_BARS as u32 * BAR + (MAX_BARS as u32 - 1) * GAP);
    }

    #[test]
    fn no_providers_still_draws_an_icon() {
        let (rgba, width, _) = render(&[], false);
        assert!(width > 0 && rgba.iter().any(|&b| b != 0));
    }
}
