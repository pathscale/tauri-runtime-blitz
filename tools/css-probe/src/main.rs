use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};

use anyrender::render_to_buffer;
use anyrender_vello_cpu::VelloCpuImageRenderer;
use blitz_dom::DocumentConfig;
use blitz_html::HtmlDocument;
use blitz_paint::paint_scene;
use blitz_traits::shell::{ColorScheme, Viewport};
use image::{ImageBuffer, Rgba};
use url::Url;

fn main() -> Result<(), String> {
    let mut args = env::args_os().skip(1);
    match args.next().as_deref().and_then(|value| value.to_str()) {
        Some("render") => {
            let input = required_path(&mut args, "input HTML")?;
            let output = required_path(&mut args, "output PNG")?;
            let width = required_u32(&mut args, "width")?;
            let height = required_u32(&mut args, "height")?;
            let background = args
                .next()
                .map(|value| parse_rgb(&value.to_string_lossy()))
                .transpose()?;
            ensure_finished(args)?;
            render(&input, &output, width, height, background)
        }
        Some("diff") => {
            let reference = required_path(&mut args, "reference PNG")?;
            let actual = required_path(&mut args, "actual PNG")?;
            let output = required_path(&mut args, "diff PNG")?;
            ensure_finished(args)?;
            diff(&reference, &actual, &output)
        }
        _ => Err(
            "usage: css-probe render <input.html> <output.png> <width> <height> [RRGGBB]\n       css-probe diff <reference.png> <actual.png> <diff.png>"
                .into(),
        ),
    }
}

fn required_path(
    args: &mut impl Iterator<Item = std::ffi::OsString>,
    name: &str,
) -> Result<PathBuf, String> {
    args.next()
        .map(PathBuf::from)
        .ok_or_else(|| format!("missing {name}"))
}

fn required_u32(
    args: &mut impl Iterator<Item = std::ffi::OsString>,
    name: &str,
) -> Result<u32, String> {
    let value = args.next().ok_or_else(|| format!("missing {name}"))?;
    value
        .to_string_lossy()
        .parse()
        .map_err(|error| format!("invalid {name}: {error}"))
}

fn ensure_finished(mut args: impl Iterator<Item = std::ffi::OsString>) -> Result<(), String> {
    if args.next().is_some() {
        Err("too many arguments".into())
    } else {
        Ok(())
    }
}

fn parse_rgb(value: &str) -> Result<[u8; 3], String> {
    let value = value.strip_prefix('#').unwrap_or(value);
    if value.len() != 6 {
        return Err("background must be a six-digit RGB hex value".into());
    }
    let parse = |range| {
        u8::from_str_radix(&value[range], 16)
            .map_err(|error| format!("invalid background color: {error}"))
    };
    Ok([parse(0..2)?, parse(2..4)?, parse(4..6)?])
}

fn render(
    input: &Path,
    output: &Path,
    width: u32,
    height: u32,
    background: Option<[u8; 3]>,
) -> Result<(), String> {
    let input = input
        .canonicalize()
        .map_err(|error| format!("failed to resolve {}: {error}", input.display()))?;
    let html = std::fs::read_to_string(&input)
        .map_err(|error| format!("failed to read {}: {error}", input.display()))?;
    let base_url = Url::from_file_path(&input).map_err(|_| "input path is not a file URL")?;
    let mut document = HtmlDocument::from_html(
        &html,
        DocumentConfig {
            base_url: Some(base_url.to_string()),
            viewport: Some(Viewport::new(width, height, 1.0, ColorScheme::Light)),
            ..Default::default()
        },
    );
    document.resolve(0.0);
    let mut buffer = render_to_buffer::<VelloCpuImageRenderer, _>(
        |scene| paint_scene(scene, &mut document, 1.0, width, height, 0, 0),
        width,
        height,
    );
    if let Some(background) = background {
        for pixel in buffer.chunks_exact_mut(4) {
            let alpha = u16::from(pixel[3]);
            for channel in 0..3 {
                pixel[channel] = ((u16::from(pixel[channel]) * alpha
                    + u16::from(background[channel]) * (255 - alpha))
                    / 255) as u8;
            }
            pixel[3] = 255;
        }
    }
    image::save_buffer_with_format(
        output,
        &buffer,
        width,
        height,
        image::ColorType::Rgba8,
        image::ImageFormat::Png,
    )
    .map_err(|error| format!("failed to write {}: {error}", output.display()))
}

fn diff(reference: &Path, actual: &Path, output: &Path) -> Result<(), String> {
    let reference = image::open(reference)
        .map_err(|error| format!("failed to read {}: {error}", reference.display()))?
        .to_rgba8();
    let actual = image::open(actual)
        .map_err(|error| format!("failed to read {}: {error}", actual.display()))?
        .to_rgba8();
    if reference.dimensions() != actual.dimensions() {
        return Err(format!(
            "image dimensions differ: reference {:?}, actual {:?}",
            reference.dimensions(),
            actual.dimensions()
        ));
    }

    let (width, height) = reference.dimensions();
    let mut diff_image = ImageBuffer::<Rgba<u8>, Vec<u8>>::new(width, height);
    let mut changed_pixels = 0_u64;
    let mut total_error = 0_u64;
    let thresholds = [4_u8, 8, 16, 32, 64];
    let mut pixels_over_threshold = [0_u64; 5];
    let mut color_pairs = HashMap::<([u8; 4], [u8; 4]), u64>::new();
    for (x, y, reference_pixel) in reference.enumerate_pixels() {
        let actual_pixel = actual.get_pixel(x, y);
        let channels = std::array::from_fn::<_, 4, _>(|index| {
            reference_pixel[index].abs_diff(actual_pixel[index])
        });
        if channels.iter().any(|channel| *channel != 0) {
            changed_pixels += 1;
            *color_pairs
                .entry((reference_pixel.0, actual_pixel.0))
                .or_default() += 1;
        }
        let max_rgb_error = channels[..3].iter().copied().max().unwrap_or(0);
        for (index, threshold) in thresholds.iter().enumerate() {
            if max_rgb_error > *threshold {
                pixels_over_threshold[index] += 1;
            }
        }
        total_error += channels
            .iter()
            .map(|channel| u64::from(*channel))
            .sum::<u64>();
        diff_image.put_pixel(x, y, Rgba([channels[0], channels[1], channels[2], 255]));
    }
    diff_image
        .save_with_format(output, image::ImageFormat::Png)
        .map_err(|error| format!("failed to write {}: {error}", output.display()))?;

    let pixels = u64::from(width) * u64::from(height);
    let mean_absolute_error = total_error as f64 / (pixels * 4) as f64;
    println!("changed_pixels={changed_pixels}/{pixels}");
    println!("mean_absolute_error={mean_absolute_error:.6}");
    for (threshold, count) in thresholds.into_iter().zip(pixels_over_threshold) {
        let percent = count as f64 * 100.0 / pixels as f64;
        println!("pixels_rgb_error_gt_{threshold}={count}/{pixels} ({percent:.4}%)");
    }
    let blurred_reference = image::imageops::blur(&reference, 4.0);
    let blurred_actual = image::imageops::blur(&actual, 4.0);
    let mut blurred_total_error = 0_u64;
    let mut blurred_over_16 = 0_u64;
    for (reference_pixel, actual_pixel) in blurred_reference.pixels().zip(blurred_actual.pixels()) {
        let rgb_error = [0, 1, 2].map(|index| reference_pixel[index].abs_diff(actual_pixel[index]));
        blurred_total_error += rgb_error
            .iter()
            .map(|channel| u64::from(*channel))
            .sum::<u64>();
        if rgb_error.into_iter().max().unwrap_or(0) > 16 {
            blurred_over_16 += 1;
        }
    }
    let blurred_mean_absolute_error = blurred_total_error as f64 / (pixels * 3) as f64;
    let blurred_over_16_percent = blurred_over_16 as f64 * 100.0 / pixels as f64;
    println!("blur4_rgb_mean_absolute_error={blurred_mean_absolute_error:.6}");
    println!(
        "blur4_pixels_rgb_error_gt_16={blurred_over_16}/{pixels} ({blurred_over_16_percent:.4}%)"
    );
    let mut color_pairs = color_pairs.into_iter().collect::<Vec<_>>();
    color_pairs.sort_unstable_by_key(|(_, count)| std::cmp::Reverse(*count));
    for ((reference, actual), count) in color_pairs.into_iter().take(8) {
        println!("color_pair={reference:?}->{actual:?} pixels={count}");
    }
    Ok(())
}
