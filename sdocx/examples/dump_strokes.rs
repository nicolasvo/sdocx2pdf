//! Dumps the properties of every stroke in a `.sdocx` file, for reverse engineering the pen
//! settings that Samsung Notes stores.
//!
//! Usage: `cargo run --example dump_strokes -- <file.sdocx>`

use std::collections::BTreeMap;

use sdocx::{Document, DocObject};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).expect("usage: dump_strokes <file.sdocx>");

    pen_stats(&path)?;
    tilt_analysis(path)?;

    Ok(())
}

fn pen_stats(path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let (document, _media) = Document::from_zip(path)?;

    // pen_name => aggregate stats over the strokes using it.
    #[derive(Default)]
    struct PenStats {
        count: usize,
        pen_sizes: std::collections::BTreeSet<String>,
        size_levels: std::collections::BTreeSet<u32>,
        original_widths: std::collections::BTreeSet<u32>,
        advanced: std::collections::BTreeSet<String>,
        tool_types: std::collections::BTreeSet<u16>,
        fixed_width: std::collections::BTreeSet<bool>,
        tilt: std::collections::BTreeSet<bool>,
        event_counts: Vec<usize>,
    }

    let mut stats: BTreeMap<String, PenStats> = BTreeMap::new();

    let mut event_sample = None;

    for (page_idx, page) in document.pages().iter().enumerate() {
        for (layer_idx, layer) in page.layers().iter().enumerate() {
            for object in layer.objects() {
                let DocObject::Stroke(stroke) = object else {
                    continue;
                };

                let name = stroke.pen_name().map(|s| s.to_string()).unwrap_or("<none>".into());
                let entry = stats.entry(name).or_default();

                entry.count += 1;
                if let Some(size) = stroke.pen_size() {
                    entry.pen_sizes.insert(format!("{size}"));
                }
                if let Some(level) = stroke.size_level() {
                    entry.size_levels.insert(level);
                }
                if let Some(w) = stroke.original_width() {
                    entry.original_widths.insert(w);
                }
                if let Some(adv) = stroke.advanced_pen_settings() {
                    entry.advanced.insert(adv.to_string());
                }
                entry.tool_types.insert(stroke.tool_type() as u16);
                entry.fixed_width.insert(stroke.is_fixed_width());
                entry.event_counts.push(stroke.events().len());

                if entry.event_counts.len() == 1 && stroke.events().len() > 3 {
                    event_sample = Some((page_idx, layer_idx, stroke));
                }
            }
        }
    }

    for (name, s) in &stats {
        println!("pen: {name}");
        println!("  strokes: {}", s.count);
        println!("  pen_size: {:?}", s.pen_sizes);
        println!("  size_level: {:?}", s.size_levels);
        println!("  original_width: {:?}", s.original_widths);
        println!("  tool_type: {:?}", s.tool_types);
        println!("  fixed_width: {:?}", s.fixed_width);
        println!("  tilt_present: {:?}", s.tilt);
        println!("  events/stroke: min {:?}, max {:?}", s.event_counts.iter().min(), s.event_counts.iter().max());
        for adv in &s.advanced {
            println!("  advanced_pen_settings: {adv:?}");
        }
        println!();
    }

    if let Some((p, l, stroke)) = event_sample {
        println!("=== first multi-event stroke (page {p}, layer {l}) ===");
        println!("tool_type = {:?}", stroke.tool_type());
        println!(
            "pen_name = {:?}, pen_size = {:?}, advanced = {:?}",
            stroke.pen_name(),
            stroke.pen_size(),
            stroke.advanced_pen_settings()
        );
        for event in stroke.events().iter().take(8) {
            println!("  {event:?}");
        }
    }

    Ok(())
}

/// Prints statistics over the tilt/orientation data recorded with each stroke. The tilt is the
/// angle between the pen and the screen normal, and the orientation is the direction in which the
/// top of the pen leans, both as radians.
fn tilt_analysis(path: String) -> Result<(), Box<dyn std::error::Error>> {
    use sdocx::page::object::DocObject;

    let (document, _media) = Document::from_zip(&path)?;

    let mut tilts = Vec::new();
    let mut orientations = Vec::new();
    let mut per_stroke_mean_ori = Vec::new();

    for page in document.pages() {
        for layer in page.layers() {
            for object in layer.objects() {
                let DocObject::Stroke(stroke) = object else { continue };
                let mut sum = 0.0f32;
                let mut n = 0;
                for event in stroke.events() {
                    if let Some(t) = &event.tilt_data {
                        tilts.push(t.tilt);
                        orientations.push(t.orientation);
                        sum += t.orientation;
                        n += 1;
                    }
                }
                if n > 0 {
                    per_stroke_mean_ori.push(sum / n as f32);
                }
            }
        }
    }

    let stats = |v: &[f32]| -> (f32, f32, f32) {
        let min = v.iter().cloned().fold(f32::INFINITY, f32::min);
        let max = v.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
        let mean = v.iter().sum::<f32>() / v.len() as f32;
        (min.to_degrees(), max.to_degrees(), mean.to_degrees())
    };

    println!("events with tilt: {}", tilts.len());
    println!("tilt (deg) min/max/mean: {:?}", stats(&tilts));
    println!("orientation (deg) min/max/mean: {:?}", stats(&orientations));

    // Histogram of per-stroke mean orientation in 30-degree buckets.
    let mut buckets = [0usize; 12];
    for o in &per_stroke_mean_ori {
        let b = ((o.to_degrees() + 180.0).rem_euclid(360.0) / 30.0) as usize % 12;
        buckets[b] += 1;
    }
    println!("per-stroke mean orientation buckets (30° each, from -180°):");
    for (i, c) in buckets.iter().enumerate() {
        println!("  {:>3}..{:>3}°: {} strokes", -180 + i as i32 * 30, -180 + (i as i32 + 1) * 30, c);
    }

    Ok(())
}
