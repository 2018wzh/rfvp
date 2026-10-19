//! ASS tracks owned by the rendering thread, driven only by native media clocks.
use std::{collections::BTreeMap, fs::File, io::Read, path::{Component, Path, PathBuf}, sync::Arc};
use anyhow::{ensure, Result};
use astra_subtitle::{AssRenderer, BitmapRun, FontData, FrameChange, VideoColorSpace};
use glam::{Mat4, Vec2, Vec3, Vec4};
use image::{DynamicImage, RgbaImage};
use serde::Deserialize;
use wgpu::util::DeviceExt;
use crate::rfvp_render::{GpuCommonResources, GpuTexture, pipelines::sprite::SpritePipeline,
    vertices::{PosColTexVertex, VertexSource}};

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Clock { Movie, Bgm }

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Description { version: u32, tracks: Vec<TrackDescription> }
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TrackDescription { clock: Clock, resource: String, ass: String, fonts: Vec<String> }
struct Track { description: TrackDescription, ass: Vec<u8>, fonts: Vec<FontData> }

#[derive(Clone, Copy)]
pub(crate) struct Media<'a> {
    pub clock: Clock,
    pub resource: &'a str,
    pub serial: u64,
    pub position_ms: Option<u64>,
    pub storage: (u32, u32),
    pub color: Option<VideoColorSpace>,
}

fn relative(path: &str) -> Result<&Path> {
    let relative = Path::new(path);
    ensure!(!path.is_empty() && path.len() <= 4096 && !path.contains([':', '\\', '\0'])
        && relative.components().all(|part| matches!(part, Component::Normal(_))),
        "RFVP_SUBTITLE_PATH_INVALID");
    Ok(relative)
}
fn read(root: &Path, path: &str, maximum: u64) -> Result<Vec<u8>> {
    let path = std::fs::canonicalize(root.join(relative(path)?))?;
    ensure!(path.starts_with(root), "RFVP_SUBTITLE_PATH_OUTSIDE_ROOT");
    let file = File::open(path)?;
    ensure!(file.metadata()?.is_file() && file.metadata()?.len() <= maximum, "RFVP_SUBTITLE_FILE_BOUNDS");
    let mut bytes = Vec::new();
    file.take(maximum + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= maximum, "RFVP_SUBTITLE_FILE_BOUNDS");
    Ok(bytes)
}
fn validate(description: &Description) -> Result<()> {
    ensure!(description.version == 1, "RFVP_SUBTITLE_MAP_VERSION");
    for (index, track) in description.tracks.iter().enumerate() {
        relative(&track.resource)?;
        relative(&track.ass)?;
        ensure!(!track.fonts.is_empty(), "RFVP_SUBTITLE_FONTS_REQUIRED");
        for font in &track.fonts { relative(font)?; }
        ensure!(!description.tracks[..index].iter().any(|old| old.clock == track.clock
            && old.resource.eq_ignore_ascii_case(&track.resource)), "RFVP_SUBTITLE_TRACK_DUPLICATE");
    }
    Ok(())
}

struct Active { track: usize, serial: u64, renderer: AssRenderer,
    last: Option<(u64, (u32, u32), (u32, u32), VideoColorSpace)> }
struct GpuRun { texture: GpuTexture, mask: DynamicImage, vertices: wgpu::Buffer, rgb: [u8; 3] }

pub(crate) struct Subtitles {
    tracks: Vec<Track>, active: Option<Active>, latest: u64, epoch: u64, runs: Vec<GpuRun>,
}
impl Subtitles {
    pub fn load(root: &str, path: &str) -> Result<Self> {
        let root = std::fs::canonicalize(root)?;
        let map = read(&root, path, 1024 * 1024)?;
        // Serde diagnostics may quote resource text; expose only the structural category.
        let description: Description = serde_json::from_slice(&map)
            .map_err(|_| anyhow::anyhow!("RFVP_SUBTITLE_MAP_INVALID"))?;
        validate(&description)?;
        let mut loaded: BTreeMap<PathBuf, FontData> = BTreeMap::new();
        let mut tracks = Vec::with_capacity(description.tracks.len());
        for description in description.tracks {
            let ass = read(&root, &description.ass, 16 * 1024 * 1024)?;
            ensure!(std::str::from_utf8(&ass).is_ok() && !ass.contains(&0), "RFVP_SUBTITLE_ASS_UTF8_INVALID");
            let mut fonts = Vec::with_capacity(description.fonts.len());
            for path in &description.fonts {
                let key = PathBuf::from(path);
                if !loaded.contains_key(&key) {
                    let bytes = read(&root, path, 64 * 1024 * 1024)?;
                    ensure!(!bytes.is_empty(), "RFVP_SUBTITLE_FONT_EMPTY");
                    loaded.insert(key.clone(), FontData { name: path.clone(), bytes: Arc::from(bytes) });
                }
                fonts.push(loaded[&key].clone());
            }
            // Parse each actual track before startup succeeds; rendering validates used glyphs.
            AssRenderer::new(&ass, &fonts)?;
            tracks.push(Track { description, ass, fonts });
        }
        Ok(Self { tracks, active: None, latest: 0, epoch: 0, runs: Vec::new() })
    }

    pub fn update<'a>(&mut self, media: impl Iterator<Item = Media<'a>>, epoch: u64,
        logical: (u32, u32), raster: (u32, u32), resources: &GpuCommonResources) -> Result<()> {
        if epoch != self.epoch {
            self.active = None;
            self.runs.clear();
            self.latest = 0;
            self.epoch = epoch;
        }
        let current_key = self.active.as_ref().map(|active| (active.track, active.serial));
        let selected = select(&self.tracks, current_key, self.latest, media);
        if let Some((track, media)) = selected.filter(|(_, media)| media.serial > self.latest) {
            self.active = None;
            self.runs.clear();
            self.latest = media.serial;
            let source = &self.tracks[track];
            self.active = Some(Active { track, serial: media.serial,
                renderer: AssRenderer::new(&source.ass, &source.fonts)?, last: None });
        }
        let Some((_, media)) = selected else {
            self.active = None;
            self.runs.clear();
            return Ok(());
        };
        let position = media.position_ms.ok_or_else(|| anyhow::anyhow!("RFVP_SUBTITLE_MEDIA_CLOCK_UNAVAILABLE"))?;
        let color = media.color.ok_or_else(|| anyhow::anyhow!("RFVP_SUBTITLE_MEDIA_COLOR_UNAVAILABLE"))?;
        let active = self.active.as_mut().expect("selected active track");
        let signature = (position, media.storage, raster, color);
        if active.last == Some(signature) { return Ok(()); }
        let frame = active.renderer.render(position, media.storage, raster)?;
        if frame.changed || active.last.is_none_or(|previous| previous.3 != color) {
            self.runs.truncate(frame.runs.len());
            for (index, run) in frame.runs.iter().enumerate() {
                let rgba = run.rgba_for_video(frame.matrix, color)?;
                let rgb = [rgba[0], rgba[1], rgba[2]];
                let created = index == self.runs.len();
                if created {
                    let mut mask = RgbaImage::new(run.w, run.h);
                    write_mask(&mut mask, run, rgb);
                    let image = DynamicImage::ImageRgba8(mask);
                    let texture = GpuTexture::new(resources, &image, Some("ASS mask"));
                    let vertices = resources.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some("ASS run vertices"), contents: bytemuck::cast_slice(&quad(run.x, run.y, run.w, run.h, rgba, logical, raster)),
                        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                    });
                    self.runs.push(GpuRun { texture, mask: image, vertices, rgb });
                }
                let gpu = &mut self.runs[index];
                if !created && (frame.change == FrameChange::Content || active.last.is_none() || gpu.rgb != rgb) {
                    if gpu.mask.width() != run.w || gpu.mask.height() != run.h {
                        gpu.mask = DynamicImage::ImageRgba8(RgbaImage::new(run.w, run.h));
                    }
                    let image = gpu.mask.as_mut_rgba8().expect("RGBA mask");
                    write_mask(image, run, rgb);
                    if !gpu.texture.update_rgba8(resources, &gpu.mask) {
                        gpu.texture = GpuTexture::new(resources, &gpu.mask, Some("ASS mask"));
                    }
                    gpu.rgb = rgb;
                }
                if !created {
                    resources.queue.write_buffer(&gpu.vertices, 0,
                        bytemuck::cast_slice(&quad(run.x, run.y, run.w, run.h, rgba, logical, raster)));
                }
            }
        }
        active.last = Some(signature);
        Ok(())
    }

    pub fn draw<'a>(&'a self, pass: &mut wgpu::RenderPass<'a>, pipeline: &'a SpritePipeline, projection: Mat4) {
        for run in &self.runs {
            pipeline.draw(pass, VertexSource::VertexBuffer { vertex_buffer: &run.vertices,
                vertices: 0..6, instances: 0..1 }, run.texture.bind_group(), projection);
        }
    }
}

fn write_mask(image: &mut RgbaImage, run: BitmapRun<'_>, rgb: [u8; 3]) {
    for y in 0..run.h as usize {
        // libass guarantees no padding after the final row.
        let row = &run.alpha[y * run.stride..y * run.stride + run.w as usize];
        for (x, coverage) in row.iter().enumerate() {
            image.put_pixel(x as u32, y as u32, image::Rgba([rgb[0], rgb[1], rgb[2], *coverage]));
        }
    }
}

// Keep only the latest mapped start event. Ending it must not resurrect an older track.
fn select<'a>(tracks: &[Track], active: Option<(usize, u64)>, latest: u64,
    media: impl Iterator<Item = Media<'a>>) -> Option<(usize, Media<'a>)> {
    let mut candidate: Option<(usize, Media<'a>)> = None;
    let mut current = None;
    for media in media {
        let Some(track) = tracks.iter().position(|track| track.description.clock == media.clock
            && track.description.resource.eq_ignore_ascii_case(media.resource)) else { continue };
        if active == Some((track, media.serial)) { current = Some((track, media)); }
        if media.serial > latest && candidate.as_ref().is_none_or(|(_, old)| old.serial < media.serial) {
            candidate = Some((track, media));
        }
    }
    candidate.or(current)
}

fn quad(x: u32, y: u32, w: u32, h: u32, rgba: [u8; 4], logical: (u32, u32), raster: (u32, u32)) -> [PosColTexVertex; 6] {
    let sx = logical.0 as f32 / raster.0 as f32;
    let sy = logical.1 as f32 / raster.1 as f32;
    // RGB lives in the sRGB texture so the native pipeline linearizes it exactly once.
    let color = Vec4::new(1.0, 1.0, 1.0, rgba[3] as f32 / 255.0);
    let vertex = |u: f32, v: f32| PosColTexVertex {
        position: Vec3::new((x as f32 + w as f32 * u) * sx, (y as f32 + h as f32 * v) * sy, 0.0),
        color, texture_coordinate: Vec2::new(u, v),
    };
    [vertex(0.0,0.0),vertex(1.0,0.0),vertex(1.0,1.0),vertex(0.0,0.0),vertex(1.0,1.0),vertex(0.0,1.0)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_upload_reads_only_coverage_and_combines_inverted_ass_alpha_once() {
        let alpha = [255, 128, 17, 18, 0, 64];
        let run = BitmapRun { x: 0, y: 0, w: 2, h: 2, stride: 4, color: 0xff000080, alpha: &alpha };
        let rgba = run.rgba_for_video(astra_subtitle::YcbcrMatrix::None, VideoColorSpace::Rgb).unwrap();
        assert_eq!(rgba, [255,0,0,127]);
        let mut image = RgbaImage::new(2,2);
        write_mask(&mut image, run, [255,0,0]);
        assert_eq!(image.into_raw(), [255,0,0,255,255,0,0,128,255,0,0,0,255,0,0,64]);
        assert_eq!(quad(0,0,2,2,rgba,(2,2),(2,2))[0].color.w, 127.0 / 255.0);
    }

    #[test]
    fn media_selection_preserves_slots_pause_seek_restart_and_stop_order() {
        let tracks = vec![
            Track { description: TrackDescription { clock: Clock::Bgm, resource: "BGM/public".into(), ass: "a.ass".into(), fonts: vec!["f.ttf".into()] }, ass: vec![], fonts: vec![] },
            Track { description: TrackDescription { clock: Clock::Movie, resource: "movie/public".into(), ass: "b.ass".into(), fonts: vec!["f.ttf".into()] }, ass: vec![], fonts: vec![] },
        ];
        let bgm = Media { clock: Clock::Bgm, resource: "bgm/public", serial: 1, position_ms: Some(1000),
            storage: (640,480), color: Some(VideoColorSpace::Rgb) };
        let movie = Media { clock: Clock::Movie, resource: "movie/public", serial: 2, position_ms: Some(2000),
            storage: (720,480), color: Some(VideoColorSpace::Bt601Tv) };
        let selected = select(&tracks, None, 0, [bgm, movie].into_iter()).unwrap();
        assert_eq!((selected.0, selected.1.serial, selected.1.position_ms), (1,2,Some(2000)));
        for _ in 0..2 {
            let paused = select(&tracks, Some((1,2)), 2, [bgm,movie].into_iter()).unwrap();
            assert_eq!(paused.1.position_ms, Some(2000));
        }
        assert!(select(&tracks, Some((1,2)), 2, [bgm].into_iter()).is_none());
        let restart = Media { serial: 3, position_ms: Some(0), ..bgm };
        assert_eq!(select(&tracks, None, 2, [restart].into_iter()).unwrap().1.position_ms, Some(0));
        // A restored timeline clears the high-water mark and renderer outside select.
        assert_eq!(select(&tracks, None, 0, [bgm].into_iter()).unwrap().1.serial, 1);
        let unrelated = Media { resource: "BGM/unmapped", serial: 4, ..bgm };
        assert_eq!(select(&tracks, Some((0,3)), 3, [restart,unrelated].into_iter()).unwrap().1.serial, 3);
    }
    #[test]
    fn map_rejects_unknown_clock_duplicates_and_nonrelative_paths() {
        assert!(serde_json::from_str::<Description>(r#"{"version":1,"tracks":[{"clock":"wall","resource":"a","ass":"a","fonts":["f"]}]}"#).is_err());
        for path in ["", "../a", "a/../b", "C:/a", "/a", "a\\b", "a\0b"] { assert!(relative(path).is_err()); }
        let description: Description = serde_json::from_str(r#"{"version":1,"tracks":[{"clock":"bgm","resource":"BGM/073","ass":"a.ass","fonts":["f.ttf"]},{"clock":"bgm","resource":"bgm/073","ass":"b.ass","fonts":["f.ttf"]}]}"#).unwrap();
        assert!(validate(&description).is_err());
    }
    #[test]
    fn raster_geometry_and_inverted_ass_opacity_are_not_video_metadata() {
        let vertices = quad(10, 20, 30, 40, [255, 0, 0, 128], (100, 100), (200, 200));
        assert_eq!(vertices[0].position, Vec3::new(5.0,10.0,0.0));
        assert_eq!(vertices[2].position, Vec3::new(20.0,30.0,0.0));
        assert_eq!(vertices[0].color.w, 128.0 / 255.0);
    }
}
