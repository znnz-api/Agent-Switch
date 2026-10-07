//! Sample the bundled SVG's SMIL animation for resvg, which renders static SVGs.
//! The source asset is retained unchanged; only in-memory frame copies are sampled.
use resvg::usvg::roxmltree;
use std::collections::HashMap;

pub const FPS: f64 = 30.0;
pub const PERIOD: f64 = 6.0;
const SOURCE: &str = include_str!("../assets/gui/svg/spinners.svg");

fn numbers(value: &str, delimiter: char) -> Vec<f64> {
    value
        .split(delimiter)
        .map(|v| v.trim().parse().expect("spinner number"))
        .collect()
}

fn seconds(value: &str) -> f64 {
    value
        .trim_end_matches('s')
        .parse()
        .expect("spinner duration")
}

// SMIL keySplines specify x/y control points, so solve x before evaluating y.
fn spline(progress: f64, controls: &[f64]) -> f64 {
    let bezier = |t: f64, a: f64, b: f64| {
        3.0 * (1.0 - t).powi(2) * t * a + 3.0 * (1.0 - t) * t * t * b + t.powi(3)
    };
    let (mut low, mut high) = (0.0, 1.0);
    for _ in 0..24 {
        let middle = (low + high) / 2.0;
        if bezier(middle, controls[0], controls[2]) < progress {
            low = middle;
        } else {
            high = middle;
        }
    }
    bezier((low + high) / 2.0, controls[1], controls[3])
}

pub fn frames() -> Vec<String> {
    let document = roxmltree::Document::parse(SOURCE).expect("bundled spinner SVG");
    let animations: Vec<_> = document
        .descendants()
        .filter(|node| node.has_tag_name("animate"))
        .collect();
    let mut starts = HashMap::new();
    // Resolve the begin+offset chain in the source, independently of XML order.
    while starts.len() < animations.len() {
        let previous = starts.len();
        for node in &animations {
            let id = node.attribute("id").expect("spinner animation id");
            let begin = node.attribute("begin").expect("spinner animation begin");
            if begin.starts_with("0;") {
                starts.insert(id, 0.0);
            } else if let Some((parent, offset)) = begin.split_once(".begin+")
                && let Some(start) = starts.get(parent).copied()
            {
                starts.insert(id, start + seconds(offset));
            }
        }
        assert!(starts.len() > previous, "unresolved spinner timing");
    }
    let first = animations
        .iter()
        .find(|node| node.attribute("begin").unwrap().starts_with("0;"))
        .unwrap();
    let restart = first.attribute("begin").unwrap().split_once(';').unwrap().1;
    let (last_id, overlap) = restart.split_once(".end-").expect("spinner repeat timing");
    let last = animations
        .iter()
        .find(|node| node.attribute("id") == Some(last_id))
        .unwrap();
    let pulse_period = starts[last_id] + seconds(last.attribute("dur").unwrap()) - seconds(overlap);
    let rotation = document
        .descendants()
        .find(|node| node.has_tag_name("animateTransform"))
        .expect("spinner rotation");
    let rotation_duration = seconds(rotation.attribute("dur").unwrap());
    assert!((rotation_duration - PERIOD).abs() < 0.0001);
    let rotations: Vec<Vec<f64>> = rotation
        .attribute("values")
        .unwrap()
        .split(';')
        .map(|value| {
            value
                .split_whitespace()
                .map(|v| v.parse().unwrap())
                .collect()
        })
        .collect();
    let group = rotation.parent().unwrap();
    let group_end = group.range().start + SOURCE[group.range().start..].find('>').unwrap();

    (0..(PERIOD * FPS) as usize)
        .map(|frame| {
            let time = frame as f64 / FPS;
            let mut edits = Vec::new();
            for node in &animations {
                let duration = seconds(node.attribute("dur").unwrap());
                let phase = (time - starts[node.attribute("id").unwrap()]).rem_euclid(pulse_period);
                let values = numbers(node.attribute("values").unwrap(), ';');
                let radius = if phase < duration {
                    let position = phase / duration * (values.len() - 1) as f64;
                    let index = position.floor() as usize;
                    let controls = numbers(
                        node.attribute("keySplines")
                            .unwrap()
                            .split(';')
                            .nth(index)
                            .unwrap(),
                        ',',
                    );
                    values[index]
                        + (values[index + 1] - values[index]) * spline(position.fract(), &controls)
                } else {
                    *values.last().unwrap()
                };
                let circle = node.parent().unwrap();
                let attribute = circle
                    .attributes()
                    .find(|attribute| attribute.name() == "r")
                    .unwrap();
                edits.push((attribute.range_value(), format!("{radius:.6}")));
            }
            let angle =
                rotations[0][0] + (rotations[1][0] - rotations[0][0]) * time / rotation_duration;
            edits.push((
                group_end..group_end,
                format!(
                    " transform=\"rotate({angle} {} {})\"",
                    rotations[0][1], rotations[0][2]
                ),
            ));
            edits.sort_by_key(|(range, _)| std::cmp::Reverse(range.start));
            let mut svg = SOURCE.to_owned();
            for (range, value) in edits {
                svg.replace_range(range, &value);
            }
            svg
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_animation_samples_pulses_and_rotation() {
        let frames = frames();
        assert_eq!(frames.len(), 180);
        let raster = |source: &str| {
            let tree = resvg::usvg::Tree::from_str(source, &Default::default()).unwrap();
            let mut image = resvg::tiny_skia::Pixmap::new(48, 48).unwrap();
            resvg::render(
                &tree,
                resvg::tiny_skia::Transform::from_scale(48.0 / 256.0, 48.0 / 256.0),
                &mut image.as_mut(),
            );
            image.data().to_vec()
        };
        let first = raster(&frames[0]);
        assert_ne!(first, raster(&frames[3]));
        assert_ne!(first, raster(&frames[30]));
        for source in &frames {
            let doc = roxmltree::Document::parse(source).unwrap();
            let radii: Vec<f64> = doc
                .descendants()
                .filter(|node| node.has_tag_name("circle"))
                .map(|node| node.attribute("r").unwrap().parse().unwrap())
                .collect();
            assert_eq!(radii.len(), 12);
            assert!(radii.iter().all(|radius| (1.0..=2.0).contains(radius)));
            assert!(radii.iter().any(|radius| *radius > 1.5));
        }
    }
}
