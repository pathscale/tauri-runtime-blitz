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
            ensure_finished(args)?;
            render(&input, &output, width, height)
        }
        Some("diff") => {
            let reference = required_path(&mut args, "reference PNG")?;
            let actual = required_path(&mut args, "actual PNG")?;
            let output = required_path(&mut args, "diff PNG")?;
            ensure_finished(args)?;
            diff(&reference, &actual, &output)
        }
        _ => Err(
            "usage: css-probe render <input.html> <output.png> <width> <height>\n       css-probe diff <reference.png> <actual.png> <diff.png>"
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

fn render(input: &Path, output: &Path, width: u32, height: u32) -> Result<(), String> {
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
    let buffer = render_to_buffer::<VelloCpuImageRenderer, _>(
        |scene| paint_scene(scene, &mut document, 1.0, width, height, 0, 0),
        width,
        height,
    );
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
    for (x, y, reference_pixel) in reference.enumerate_pixels() {
        let actual_pixel = actual.get_pixel(x, y);
        let channels = std::array::from_fn::<_, 4, _>(|index| {
            reference_pixel[index].abs_diff(actual_pixel[index])
        });
        if channels.iter().any(|channel| *channel != 0) {
            changed_pixels += 1;
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
    Ok(())
}
