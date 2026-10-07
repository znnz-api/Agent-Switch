//! Rasterized frames for the animated protocol-conversion icon.
//! `resvg` does not advance SMIL animations in an egui texture, so sample the
//! small rotate animation once and let the GUI select a frame by time.

use resvg::usvg::roxmltree;

pub const FPS: f64 = 30.0;
pub const PERIOD: f64 = 2.4;
const SOURCE: &str = include_str!("../assets/gui/svg/Converting.svg");

pub fn frames() -> Vec<String> {
    let document = roxmltree::Document::parse(SOURCE).expect("bundled converting SVG");
    let animation = document
        .descendants()
        .find(|node| node.has_tag_name("animateTransform"))
        .expect("converting rotation animation");
    let from = animation
        .attribute("from")
        .and_then(|value| value.split_whitespace().next())
        .and_then(|value| value.parse::<f64>().ok())
        .unwrap_or(0.0);
    let to = animation
        .attribute("to")
        .and_then(|value| value.split_whitespace().next())
        .and_then(|value| value.parse::<f64>().ok())
        .unwrap_or(-360.0);
    let group = animation.parent().expect("converting animation parent");
    let open_end = group.range().start
        + SOURCE[group.range().start..]
            .find('>')
            .expect("converting group opening tag");
    (0..(PERIOD * FPS) as usize)
        .map(|frame| {
            let progress = frame as f64 / (PERIOD * FPS);
            let angle = from + (to - from) * progress;
            let mut svg = SOURCE.to_owned();
            svg.insert_str(
                open_end,
                &format!(" transform=\"rotate({angle:.4} 12 12)\""),
            );
            svg
        })
        .collect()
}
