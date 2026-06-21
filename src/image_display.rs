use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImageDisplayMode {
    Auto,
    Inline,
    Path,
    Off,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InlineImageResult {
    Rendered,
    Skipped,
    Failed(String),
}

pub fn save_base64_png(runtime_dir: &Path, filename: &str, data: &str) -> Result<PathBuf> {
    let png = BASE64
        .decode(data)
        .context("failed to decode browser screenshot")?;
    fs::create_dir_all(runtime_dir)?;
    let path = runtime_dir.join(filename);
    fs::write(&path, png).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(path)
}

pub fn render_inline(path: &Path) -> InlineImageResult {
    match image_display_mode() {
        ImageDisplayMode::Off | ImageDisplayMode::Path => InlineImageResult::Skipped,
        ImageDisplayMode::Auto => match render_inline_impl(path) {
            Ok(()) => InlineImageResult::Rendered,
            Err(_) => InlineImageResult::Skipped,
        },
        ImageDisplayMode::Inline => match render_inline_impl(path) {
            Ok(()) => InlineImageResult::Rendered,
            Err(error) => InlineImageResult::Failed(error.to_string()),
        },
    }
}

fn image_display_mode() -> ImageDisplayMode {
    image_display_mode_from_str(std::env::var("EMISSARY_IMAGE_DISPLAY").ok().as_deref())
}

fn image_display_mode_from_str(value: Option<&str>) -> ImageDisplayMode {
    match value.map(str::trim).map(str::to_lowercase).as_deref() {
        Some("inline") => ImageDisplayMode::Inline,
        Some("path") => ImageDisplayMode::Path,
        Some("off") => ImageDisplayMode::Off,
        _ => ImageDisplayMode::Auto,
    }
}

#[cfg(feature = "terminal-images")]
fn render_inline_impl(path: &Path) -> Result<()> {
    use std::io::IsTerminal;

    if !std::io::stdout().is_terminal() {
        anyhow::bail!("stdout is not an interactive terminal");
    }

    let config = viuer::Config {
        transparent: true,
        absolute_offset: false,
        restore_cursor: true,
        ..Default::default()
    };
    viuer::print_from_file(path, &config)
        .map(|_| ())
        .with_context(|| format!("failed to render {} in terminal", path.display()))
}

#[cfg(not(feature = "terminal-images"))]
fn render_inline_impl(_path: &Path) -> Result<()> {
    anyhow::bail!("terminal image support was disabled at build time");
}

#[cfg(test)]
mod tests {
    use super::{ImageDisplayMode, image_display_mode_from_str};

    #[test]
    fn defaults_to_auto_image_display() {
        assert_eq!(image_display_mode_from_str(None), ImageDisplayMode::Auto);
    }

    #[test]
    fn parses_image_display_mode() {
        assert_eq!(
            image_display_mode_from_str(Some("path")),
            ImageDisplayMode::Path
        );
    }
}
