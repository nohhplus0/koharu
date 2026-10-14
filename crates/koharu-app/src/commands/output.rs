use anyhow::{Context as _, Result};
use futures::future::try_join_all;
use image::{
    ExtendedColorType, ImageEncoder as _,
    codecs::{jpeg::JpegEncoder, png::{CompressionType, FilterType, PngEncoder}},
};
use koharu_psd::{PsdExportOptions, export_page};
use koharu_rasterizer::{Raster, RasterOptions, Rasterizer};
use koharu_renderer::{Frame, Renderer};
use koharu_scene::{AssetRole, EntityId, Snapshot};
use rayon::prelude::*;
use serde::Deserialize;
use specta::Type;
use std::{io::Write as _, sync::Arc};
use tauri::{State, WebviewWindow, ipc::IpcResponse};
use tauri_runtime_cef::CefRuntime;

use super::{Error, project::CurrentProject};
use koharu_desktop::Desktop;

const THUMBNAIL_EDGE: u32 = 128;

#[derive(Type)]
#[specta(transparent)]
pub(crate) struct ThumbnailBytes(#[specta(type = Vec<u8>)] Vec<u8>);

impl IpcResponse for ThumbnailBytes {
    fn body(self) -> tauri::Result<tauri::ipc::InvokeResponseBody> {
        Ok(self.0.into())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum ExportFormat {
    Png,
    Jpg,
    Psd,
    PngCbz,
    JpgCbz,
    PngZip,
    JpgZip,
}

#[tracing::instrument(
    target = "koharu_metrics",
    name = "export",
    skip_all,
    fields(origin = "user", format = ?format),
)]
#[tauri::command]
#[specta::specta]
pub(crate) async fn export(
    window: WebviewWindow<CefRuntime>,
    format: ExportFormat,
    quality: Option<u8>,
    project: State<'_, CurrentProject>,
    desktop: State<'_, Desktop>,
) -> std::result::Result<(), Error> {
    let (name, snapshot) = {
        let project = project.project.lock().await;
        let project = project.as_ref().context("no project is open")?;
        (project.name.clone(), project.snapshot())
    };
    let pages = snapshot.pages().map(|page| page.id()).collect::<Vec<_>>();
    if pages.is_empty() {
        return Err(anyhow::anyhow!("there are no pages to export").into());
    }
    let mut group_names = Vec::<String>::new();
    for &page_id in &pages {
        let page = snapshot.page(page_id)?.page()?;
        let group = page
            .label
            .split_once(super::import::ARCHIVE_GROUP_SEPARATOR)
            .map_or(name.as_str(), |(group, _)| group);
        if !group_names.iter().any(|existing| existing == group) {
            group_names.push(group.to_owned());
        }
    }

    let dialog = rfd::AsyncFileDialog::new().set_parent(&window);
    let destination = match format {
        ExportFormat::Png | ExportFormat::Jpg | ExportFormat::Psd => dialog.pick_folder().await,
        ExportFormat::PngCbz | ExportFormat::JpgCbz | ExportFormat::PngZip | ExportFormat::JpgZip
            if group_names.len() > 1 => dialog.pick_folder().await,
        ExportFormat::PngCbz | ExportFormat::JpgCbz => {
            dialog
                .add_filter("Comic Book Archive", &["cbz"])
                .set_file_name(format!("{name}.cbz"))
                .save_file()
                .await
        }
        ExportFormat::PngZip | ExportFormat::JpgZip => {
            dialog
                .add_filter("ZIP Archive", &["zip"])
                .set_file_name(format!("{name}.zip"))
                .save_file()
                .await
        }
    };
    let Some(destination) = destination.map(|destination| destination.path().to_owned()) else {
        return Ok(());
    };
    let jpeg_quality = quality.unwrap_or(90);
    if matches!(format, ExportFormat::Jpg | ExportFormat::JpgCbz | ExportFormat::JpgZip)
        && !(1..=100).contains(&jpeg_quality)
    {
        return Err(anyhow::anyhow!("JPEG quality must be between 1 and 100").into());
    }
    let renderer = desktop.renderer();
    let rasterizer = desktop.rasterizer().await?;

    // Archive exports are processed one page at a time. Keeping all rendered frames
    // and encoded images in vectors makes memory usage grow with the whole project.
    if matches!(
        format,
        ExportFormat::PngCbz | ExportFormat::JpgCbz | ExportFormat::PngZip | ExportFormat::JpgZip
    ) {
        let extension = match format {
            ExportFormat::PngCbz | ExportFormat::PngZip => "png",
            _ => "jpg",
        };
        let archive_extension = match format {
            ExportFormat::PngZip | ExportFormat::JpgZip => "zip",
            _ => "cbz",
        };
        let multiple_groups = group_names.len() > 1;

        for group in &group_names {
            // Select one group's page IDs and labels without retaining image data.
            let mut group_pages = Vec::new();
            for &page_id in &pages {
                let page = snapshot.page(page_id)?.page()?;
                let (page_group, label) = page
                    .label
                    .split_once(super::import::ARCHIVE_GROUP_SEPARATOR)
                    .map(|(group, label)| (group, label))
                    .unwrap_or((name.as_str(), page.label.as_str()));
                if page_group == group {
                    group_pages.push((page_id, label.to_owned()));
                }
            }

            let archive_destination = if multiple_groups {
                let safe_group = group
                    .trim()
                    .trim_end_matches(|character: char| character == '.' || character.is_whitespace())
                    .replace(['<', '>', ':', '"', '/', '\\', '|', '?', '*'], "_");
                let base_name = if safe_group.is_empty() { "comic" } else { &safe_group };
                let candidate = destination.join(format!("{base_name}.{archive_extension}"));
                if candidate.exists() {
                    let mut suffix = 1;
                    loop {
                        let candidate = destination.join(format!(
                            "{base_name} ({suffix}).{archive_extension}"
                        ));
                        if !candidate.exists() {
                            break candidate;
                        }
                        suffix += 1;
                    }
                } else {
                    candidate
                }
            } else {
                destination.clone()
            };

            let directory = archive_destination
                .parent()
                .context("archive path has no parent")?;
            let mut archive =
                zip::ZipWriter::new(tempfile::NamedTempFile::new_in(directory)?);
            let width = group_pages.len().to_string().len().max(4);

            for (index, (page_id, label)) in group_pages.into_iter().enumerate() {
                let frame = renderer.render(&snapshot, page_id).await?;
                let rasterizer = Arc::clone(&rasterizer);
                let image = tokio_rayon::spawn(move || {
                    rasterizer
                        .rasterize(&frame.raster_frame()?, RasterOptions::default())
                        .map(|raster| raster.image)
                })
                .await??;

                let bytes = tokio_rayon::spawn(move || -> Result<Vec<u8>> {
                    let mut bytes = Vec::new();
                    match format {
                        ExportFormat::PngCbz | ExportFormat::PngZip => {
                            PngEncoder::new_with_quality(
                                &mut bytes,
                                CompressionType::Best,
                                FilterType::Adaptive,
                            )
                            .write_image(
                                image.as_raw(),
                                image.width(),
                                image.height(),
                                ExtendedColorType::Rgba8,
                            )?;
                        }
                        ExportFormat::JpgCbz | ExportFormat::JpgZip => {
                            let image = image::DynamicImage::ImageRgba8(image).to_rgb8();
                            JpegEncoder::new_with_quality(&mut bytes, jpeg_quality).write_image(
                                image.as_raw(),
                                image.width(),
                                image.height(),
                                ExtendedColorType::Rgb8,
                            )?;
                        }
                        _ => unreachable!("archive format checked above"),
                    }
                    Ok(bytes)
                })
                .await??;

                let page_name = label
                    .trim()
                    .trim_end_matches(|character: char| character == '.' || character.is_whitespace());
                let page_name = page_name
                    .rsplit_once('.')
                    .map_or(page_name, |(stem, _)| stem)
                    .replace(['<', '>', ':', '"', '/', '\\', '|', '?', '*'], "_");
                let page_name = format!(
                    "{:0width$}_{}.{}",
                    index + 1,
                    if page_name.is_empty() { "page" } else { &page_name },
                    extension
                );
                let options = zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Stored);
                archive.start_file(page_name, options)?;
                archive.write_all(&bytes)?;
                tracing::info!(
                    target: "koharu_metrics",
                    metric = "page_exported",
                    format = ?format,
                );
                // Drop the encoded image bytes before rendering the next page.
            }

            archive.finish()?.persist(archive_destination)?;
        }
        return Ok(());
    }

    let frames = try_join_all(pages.iter().map(|&page| renderer.render(&snapshot, page))).await?;
    let (extension, images) = match format {
        ExportFormat::Png | ExportFormat::PngCbz | ExportFormat::PngZip => {
            let images = tokio_rayon::spawn(move || {
                frames
                    .par_iter()
                    .map(|frame| -> Result<_> {
                        let image = rasterizer
                            .rasterize(&frame.raster_frame()?, RasterOptions::default())?
                            .image;
                        let mut bytes = Vec::new();
                        PngEncoder::new_with_quality(
                            &mut bytes,
                            CompressionType::Best,
                            FilterType::Adaptive,
                        )
                        .write_image(
                            image.as_raw(),
                            image.width(),
                            image.height(),
                            ExtendedColorType::Rgba8,
                        )?;
                        Ok(bytes)
                    })
                    .collect::<Result<Vec<_>>>()
            })
            .await?;
            ("png", images)
        }
        ExportFormat::Jpg | ExportFormat::JpgCbz | ExportFormat::JpgZip => {
            let images = tokio_rayon::spawn(move || {
                frames
                    .par_iter()
                    .map(|frame| -> Result<_> {
                        let image = rasterizer
                            .rasterize(&frame.raster_frame()?, RasterOptions::default())?
                            .image;
                        let image = image::DynamicImage::ImageRgba8(image).to_rgb8();
                        let mut bytes = Vec::new();
                        JpegEncoder::new_with_quality(&mut bytes, jpeg_quality).write_image(
                            image.as_raw(),
                            image.width(),
                            image.height(),
                            ExtendedColorType::Rgb8,
                        )?;
                        Ok(bytes)
                    })
                    .collect::<Result<Vec<_>>>()
            })
            .await?;
            ("jpg", images)
        }
        ExportFormat::Psd => {
            let options = PsdExportOptions::default();
            let images = try_join_all(
                frames
                    .iter()
                    .map(|frame| export_page(Arc::clone(&rasterizer), &snapshot, frame, &options)),
            )
            .await?;
            ("psd", images)
        }
    };
    tokio_rayon::spawn(move || -> Result<()> {
        if matches!(
            format,
            ExportFormat::PngCbz | ExportFormat::JpgCbz | ExportFormat::PngZip | ExportFormat::JpgZip
        ) {
            let mut groups: Vec<(String, Vec<(String, Vec<u8>)>)> = Vec::new();

            // Preserve the original archive boundary while keeping page order within each group.
            for (page_id, bytes) in pages.into_iter().zip(images) {
                let page = snapshot.page(page_id)?.page()?;
                let (group, label) = page
                    .label
                    .split_once(super::import::ARCHIVE_GROUP_SEPARATOR)
                    .map(|(group, label)| (group.to_owned(), label.to_owned()))
                    .unwrap_or_else(|| (name.clone(), page.label.clone()));
                let index = groups
                    .iter()
                    .position(|(existing, _)| existing == &group)
                    .unwrap_or_else(|| {
                        groups.push((group.clone(), Vec::new()));
                        groups.len() - 1
                    });
                groups[index].1.push((label, bytes));
            }

            let multiple_groups = groups.len() > 1;
            for (group, entries) in groups {
                let archive_destination = if multiple_groups {
                    let safe_group = group
                        .trim()
                        .trim_end_matches(|character: char| character == '.' || character.is_whitespace())
                        .replace(['<', '>', ':', '"', '/', '\\', '|', '?', '*'], "_");
                    let archive_extension = match format {
                        ExportFormat::PngZip | ExportFormat::JpgZip => "zip",
                        _ => "cbz",
                    };
                    let base_name = if safe_group.is_empty() { "comic" } else { &safe_group };
                    let candidate = destination.join(format!("{base_name}.{archive_extension}"));
                    if candidate.exists() {
                        let mut suffix = 1;
                        loop {
                            let candidate = destination.join(format!(
                                "{base_name} ({suffix}).{archive_extension}"
                            ));
                            if !candidate.exists() {
                                break candidate;
                            }
                            suffix += 1;
                        }
                    } else {
                        candidate
                    }
                } else {
                    destination.clone()
                };
                let directory = archive_destination
                    .parent()
                    .context("archive path has no parent")?;
                let mut archive =
                    zip::ZipWriter::new(tempfile::NamedTempFile::new_in(directory)?);
                let width = entries.len().to_string().len().max(4);

                for (index, (label, bytes)) in entries.into_iter().enumerate() {
                    let page_name = label
                        .trim()
                        .trim_end_matches(|character: char| character == '.' || character.is_whitespace());
                    let page_name = page_name
                        .rsplit_once('.')
                        .map_or(page_name, |(stem, _)| stem)
                        .replace(['<', '>', ':', '"', '/', '\\', '|', '?', '*'], "_");
                    let page_name = format!(
                        "{:0width$}_{}.{}",
                        index + 1,
                        if page_name.is_empty() { "page" } else { &page_name },
                        extension
                    );
                    // Encoded image data is already compressed.
                    let options = zip::write::SimpleFileOptions::default()
                        .compression_method(zip::CompressionMethod::Stored);
                    archive.start_file(page_name, options)?;
                    archive.write_all(&bytes)?;
                    tracing::info!(
                        target: "koharu_metrics",
                        metric = "page_exported",
                        format = ?format,
                    );
                }

                archive.finish()?.persist(archive_destination)?;
            }
        } else {
            // Async preparation and indexed encoding both preserve project order.
            let width = pages.len().to_string().len().max(4);
            for (index, (page_id, bytes)) in pages.into_iter().zip(images).enumerate() {
                let page = snapshot.page(page_id)?.page()?;
                let label = page
                    .label
                    .split_once(super::import::ARCHIVE_GROUP_SEPARATOR)
                    .map_or(page.label.as_str(), |(_, label)| label);
                let page_name = label
                    .trim()
                    .trim_end_matches(|character: char| character == '.' || character.is_whitespace());
                let page_name = page_name
                    .rsplit_once('.')
                    .map_or(page_name, |(stem, _)| stem)
                    .replace(['<', '>', ':', '"', '/', '\\', '|', '?', '*'], "_");
                let page_name = format!(
                    "{:0width$}_{}.{extension}",
                    index + 1,
                    if page_name.is_empty() { "page" } else { &page_name }
                );
                std::fs::write(destination.join(page_name), bytes)?;
                tracing::info!(
                    target: "koharu_metrics",
                    metric = "page_exported",
                    format = ?format,
                );
            }
        }
        Ok(())
    })
    .await?;
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub(crate) async fn get_thumbnail(
    page: EntityId,
    project: State<'_, CurrentProject>,
) -> std::result::Result<ThumbnailBytes, Error> {
    let snapshot = project
        .project
        .lock()
        .await
        .as_ref()
        .context("no project is open")?
        .snapshot();
    snapshot.page(page)?;
    let blob = snapshot
        .asset(page, &AssetRole::new("source")?)?
        .with_context(|| format!("page {page} has no source image"))?
        .blob;
    let bytes = snapshot.read_blob(blob).await?;
    let bytes = tokio_rayon::spawn(move || -> Result<Vec<u8>> {
        let image = image::load_from_memory(&bytes).context("failed to decode source image")?;
        if image.width() == 0 || image.height() == 0 {
            return Err(anyhow::anyhow!("source image is empty"));
        }
        let image = image.thumbnail(THUMBNAIL_EDGE, THUMBNAIL_EDGE).to_rgba8();
        let encoder = webp::Encoder::from_rgba(image.as_raw(), image.width(), image.height());
        Ok(encoder.encode(80.0).to_vec())
    })
    .await?;
    Ok(ThumbnailBytes(bytes))
}

pub(crate) async fn rendered_preview(
    renderer: &Renderer,
    rasterizer: Arc<Rasterizer>,
    snapshot: &Snapshot,
    page: EntityId,
) -> Result<Vec<u8>> {
    snapshot.page(page)?;
    let frame = renderer.render(snapshot, page).await?;
    let image = rasterize(rasterizer, &frame, RasterOptions::default())
        .await?
        .image;
    tokio_rayon::spawn(move || {
        let image = image::DynamicImage::ImageRgba8(image)
            .resize(1024, 1024, image::imageops::FilterType::Lanczos3)
            .to_rgba8();
        let encoder = webp::Encoder::from_rgba(image.as_raw(), image.width(), image.height());
        Ok::<_, anyhow::Error>(encoder.encode(85.0).to_vec())
    })
    .await
}

async fn rasterize(
    rasterizer: Arc<Rasterizer>,
    frame: &Frame,
    options: RasterOptions,
) -> Result<Raster> {
    let frame = frame.raster_frame()?;
    tokio_rayon::spawn(move || rasterizer.rasterize(&frame, options))
        .await
        .map_err(Into::into)
}
