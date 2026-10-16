use std::collections::HashMap;

#[cfg(feature = "cpu_fix")]
use egui::epaint::Mesh16;
use egui::epaint::Primitive;
use egui::{ClippedPrimitive, ImageData, Pos2, TextureId, TexturesDelta};
use skia_safe::vertices::VertexMode;
use skia_safe::{
    AlphaType, BlendMode, Canvas, ClipOp, Color, ColorType, ConditionallySend, Data, Drawable,
    Image, ImageInfo, Paint, PictureRecorder, Point, Rect, Sendable, Vertices, images, scalar,
    surfaces,
};

/// Logs at warn level the first time a call site runs, and at debug level after that.
macro_rules! warn_once {
    ($($arg:tt)+) => {{
        static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            log::debug!($($arg)+);
        } else {
            log::warn!($($arg)+);
        }
    }};
}

#[derive(Eq, PartialEq)]
enum PaintType {
    Image,
    Font,
}

struct PaintHandle {
    paint: Paint,
    image: Image,
    paint_type: PaintType,
}

pub struct Painter {
    paints: HashMap<TextureId, PaintHandle>,
    white_paint_workaround: Paint,
}

impl Painter {
    pub fn new() -> Painter {
        let mut white_paint_workaround = Paint::default();
        white_paint_workaround.set_color(Color::WHITE);

        Self {
            paints: HashMap::default(),
            white_paint_workaround,
        }
    }

    pub fn paint_and_update_textures(
        &mut self,
        canvas: &Canvas,
        dpi: f32,
        primitives: Vec<ClippedPrimitive>,
        mut textures_delta: TexturesDelta,
    ) {
        // Flatten the per-texture delta lists, keeping each texture's deltas in order.
        let queued = textures_delta
            .set
            .iter()
            .flat_map(|(id, deltas)| deltas.iter().map(move |delta| (id, delta)));

        queued.for_each(|(id, image_delta)| {
            let ImageData::Color(color_image) = &image_delta.image;

            let Some(delta_image) = images::raster_from_data(
                &ImageInfo::new(
                    (color_image.width() as i32, color_image.height() as i32),
                    ColorType::RGBA8888,
                    AlphaType::Premul,
                    None,
                ),
                Data::new_copy(
                    color_image
                        .pixels
                        .iter()
                        .flat_map(|p| p.to_array())
                        .collect::<Vec<_>>()
                        .as_slice(),
                ),
                color_image.width() * 4,
            ) else {
                warn_once!(
                    "egui_skia: skipping {:?} update; no {}x{} image",
                    id,
                    color_image.width(),
                    color_image.height()
                );
                return;
            };

            let image = match image_delta.pos {
                None => delta_image,
                Some(pos) => {
                    let Some(old_image) = self.paints.get(id).map(|handle| handle.image.clone())
                    else {
                        warn_once!("egui_skia: skipping partial update of unknown texture {id:?}");
                        return;
                    };

                    let Some(mut surface) = surfaces::raster_n32_premul(skia_safe::ISize::new(
                        old_image.width(),
                        old_image.height(),
                    )) else {
                        warn_once!(
                            "egui_skia: skipping partial update of {:?}; no {}x{} surface",
                            id,
                            old_image.width(),
                            old_image.height()
                        );
                        return;
                    };

                    let canvas = surface.canvas();

                    canvas.draw_image(&old_image, Point::new(0.0, 0.0), None);

                    canvas.clip_rect(
                        Rect::new(
                            pos[0] as scalar,
                            pos[1] as scalar,
                            (pos[0] as i32 + delta_image.width()) as scalar,
                            (pos[1] as i32 + delta_image.height()) as scalar,
                        ),
                        ClipOp::default(),
                        false,
                    );

                    canvas.clear(Color::TRANSPARENT);
                    canvas.draw_image(&delta_image, Point::new(pos[0] as f32, pos[1] as f32), None);

                    surface.image_snapshot()
                }
            };

            // TextureId::Managed(0) is the font atlas; everything else is a user/image texture.
            let paint_type = if *id == TextureId::default() {
                PaintType::Font
            } else {
                PaintType::Image
            };

            let local_matrix =
                skia_safe::Matrix::scale((1.0 / image.width() as f32, 1.0 / image.height() as f32));

            #[cfg(feature = "cpu_fix")]
            let sampling_options = skia_safe::SamplingOptions::new(
                skia_safe::FilterMode::Nearest,
                skia_safe::MipmapMode::None,
            );
            #[cfg(not(feature = "cpu_fix"))]
            let sampling_options = {
                use egui::TextureFilter;
                let filter_mode = match image_delta.options.magnification {
                    TextureFilter::Nearest => skia_safe::FilterMode::Nearest,
                    TextureFilter::Linear => skia_safe::FilterMode::Linear,
                };
                let mm_mode = match image_delta.options.minification {
                    TextureFilter::Nearest => skia_safe::MipmapMode::Nearest,
                    TextureFilter::Linear => skia_safe::MipmapMode::Linear,
                };

                skia_safe::SamplingOptions::new(filter_mode, mm_mode)
            };
            let tile_mode = skia_safe::TileMode::Clamp;

            let Some(font_shader) =
                image.to_shader((tile_mode, tile_mode), sampling_options, &local_matrix)
            else {
                warn_once!("egui_skia: skipping {id:?} update; no shader for its image");
                return;
            };

            let mut paint = Paint::default();
            paint.set_shader(font_shader);
            paint.set_color(Color::WHITE);

            self.paints.insert(
                *id,
                PaintHandle {
                    paint,
                    image,
                    paint_type,
                },
            );
        });

        for primitive in primitives {
            let skclip_rect = Rect::new(
                primitive.clip_rect.min.x,
                primitive.clip_rect.min.y,
                primitive.clip_rect.max.x,
                primitive.clip_rect.max.y,
            );
            match primitive.primitive {
                Primitive::Mesh(mut mesh) => {
                    // Drops a trailing partial triangle.
                    mesh.indices.truncate(mesh.indices.len() / 3 * 3);
                    if mesh.indices.is_empty() || !mesh.is_valid() {
                        warn_once!(
                            "egui_skia: skipping mesh with no or out-of-range indices ({} vertices, {} indices)",
                            mesh.vertices.len(),
                            mesh.indices.len()
                        );
                        continue;
                    }
                    let Some(handle) = self.paints.get(&mesh.texture_id) else {
                        warn_once!(
                            "egui_skia: skipping mesh with unknown texture {:?}",
                            mesh.texture_id
                        );
                        continue;
                    };

                    canvas.set_matrix(&skia_safe::M44::new_identity().set_scale(dpi, dpi, 1.0));
                    let arc = skia_safe::AutoCanvasRestore::guard(canvas, true);

                    #[cfg(feature = "cpu_fix")]
                    let meshes = mesh
                        .split_to_u16()
                        .into_iter()
                        .flat_map(|mesh| self.split_texture_meshes(mesh))
                        .collect::<Vec<Mesh16>>();
                    #[cfg(not(feature = "cpu_fix"))]
                    let meshes = mesh.split_to_u16();

                    for mesh in &meshes {
                        let mut pos = Vec::with_capacity(mesh.vertices.len());
                        let mut texs = Vec::with_capacity(mesh.vertices.len());
                        let mut colors = Vec::with_capacity(mesh.vertices.len());

                        mesh.vertices.iter().for_each(|v| {
                            // Vertices can be NaN; replacing them with 0 avoids dropped meshes.
                            // https://github.com/lucasmerlin/egui_skia/issues/4
                            let fixed_pos = if v.pos.x.is_nan() || v.pos.y.is_nan() {
                                Pos2::new(0.0, 0.0)
                            } else {
                                v.pos
                            };

                            pos.push(Point::new(fixed_pos.x, fixed_pos.y));
                            texs.push(Point::new(v.uv.x, v.uv.y));

                            let c = v.color;
                            let c = Color::from_argb(c.a(), c.r(), c.g(), c.b());
                            // Un-premultiply color so the Modulate blend produces the right result.
                            // https://github.com/lucasmerlin/egui_skia/issues/6
                            let mut cf = skia_safe::Color4f::from(c);
                            if cf.a > 0.0 {
                                cf.r /= cf.a;
                                cf.g /= cf.a;
                                cf.b /= cf.a;
                            }
                            colors.push(Color::from_argb(
                                c.a(),
                                (cf.r * 255.0) as u8,
                                (cf.g * 255.0) as u8,
                                (cf.b * 255.0) as u8,
                            ));
                        });

                        let vertices = Vertices::new_copy(
                            VertexMode::Triangles,
                            &pos,
                            &texs,
                            &colors,
                            Some(mesh.indices.as_slice()),
                        );

                        arc.clip_rect(skclip_rect, ClipOp::default(), true);

                        // egui uses uv 0,0 to fetch a white texel for solid-color shapes.
                        // Skia cannot fetch a color when all uv coordinates are equal, so
                        // split_texture_meshes isolates 0,0-only sub-meshes and we paint them
                        // with a solid white paint instead of the texture shader.
                        // https://bugs.chromium.org/p/skia/issues/detail?id=13706
                        let cpu_fix = cfg!(feature = "cpu_fix")
                            && handle.paint_type == PaintType::Font
                            && texs.iter().all(|point| point.x == 0.0 && point.y == 0.0);

                        let paint = if cpu_fix {
                            &self.white_paint_workaround
                        } else {
                            &handle.paint
                        };

                        arc.draw_vertices(&vertices, BlendMode::Modulate, paint);
                    }
                }
                Primitive::Callback(data) => {
                    let Some(callback) = data.callback.downcast_ref::<EguiSkiaPaintCallback>()
                    else {
                        warn_once!(
                            "egui_skia: skipping paint callback that is not an EguiSkiaPaintCallback"
                        );
                        continue;
                    };
                    let rect = data.rect;

                    let skia_rect = Rect::new(
                        rect.min.x * dpi,
                        rect.min.y * dpi,
                        rect.max.x * dpi,
                        rect.max.y * dpi,
                    );

                    let mut drawable: Drawable = (callback.callback)(skia_rect).0.into_inner();

                    let arc = skia_safe::AutoCanvasRestore::guard(canvas, true);

                    arc.clip_rect(skclip_rect, ClipOp::default(), true);
                    arc.translate((rect.min.x, rect.min.y));

                    drawable.draw(&arc, None);
                }
            }
        }

        textures_delta.free.iter().for_each(|id| {
            self.paints.remove(id);
        });

        // TexturesDelta debug-asserts it is empty when dropped.
        textures_delta.clear();
    }

    #[cfg(feature = "cpu_fix")]
    fn split_texture_meshes(&self, mesh: Mesh16) -> Vec<Mesh16> {
        if self
            .paints
            .get(&mesh.texture_id)
            .is_none_or(|handle| handle.paint_type != PaintType::Font)
        {
            return vec![mesh];
        }

        // Most vertices a sub-mesh can hold while its indices fit in a u16.
        const MAX_VERTICES: usize = u16::MAX as usize + 1;

        let mut meshes: Vec<Mesh16> = Vec::new();
        let mut run_is_zero = None;
        for triangle in mesh.indices.chunks_exact(3) {
            let (Some(a), Some(b), Some(c)) = (
                mesh.vertices.get(triangle[0] as usize),
                mesh.vertices.get(triangle[1] as usize),
                mesh.vertices.get(triangle[2] as usize),
            ) else {
                continue;
            };
            let is_zero = [a, b, c].into_iter().all(|vertex| vertex.uv == Pos2::ZERO);
            let full = meshes
                .last()
                .is_none_or(|last| last.vertices.len() + 3 > MAX_VERTICES);
            if full || run_is_zero != Some(is_zero) {
                meshes.push(Mesh16 {
                    indices: vec![],
                    vertices: vec![],
                    texture_id: mesh.texture_id,
                });
                run_is_zero = Some(is_zero);
            }
            if let Some(last) = meshes.last_mut() {
                for vertex in [a, b, c] {
                    last.indices.push(last.vertices.len() as u16);
                    last.vertices.push(*vertex);
                }
            }
        }

        meshes
    }
}

impl Default for Painter {
    fn default() -> Self {
        Self::new()
    }
}

pub struct EguiSkiaPaintCallback {
    callback: Box<dyn Fn(Rect) -> SyncSendableDrawable + Send + Sync>,
}

impl EguiSkiaPaintCallback {
    pub fn new<F: Fn(&Canvas) + Send + Sync + 'static>(callback: F) -> EguiSkiaPaintCallback {
        EguiSkiaPaintCallback {
            callback: Box::new(move |rect| {
                let mut pr = PictureRecorder::new();
                let canvas = pr.begin_recording(rect, false);
                callback(canvas);
                SyncSendableDrawable(
                    pr.finish_recording_as_drawable()
                        .unwrap()
                        .wrap_send()
                        .unwrap(),
                )
            }),
        }
    }
}

struct SyncSendableDrawable(pub Sendable<Drawable>);

unsafe impl Sync for SyncSendableDrawable {}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use egui::epaint::{ImageDelta, Mesh, PaintCallback, Vertex};
    use egui::{Color32, ColorImage, TextureOptions, pos2};

    use super::*;

    fn rect(x0: f32, y0: f32, x1: f32, y1: f32) -> egui::Rect {
        egui::Rect::from_min_max(pos2(x0, y0), pos2(x1, y1))
    }

    /// Paints onto a transparent surface and returns its pixels as RGBA bytes.
    fn paint(
        width: i32,
        height: i32,
        primitives: Vec<Primitive>,
        textures_delta: TexturesDelta,
    ) -> Vec<u8> {
        let mut surface = surfaces::raster_n32_premul((width, height)).expect("raster surface");
        let clip_rect = rect(0.0, 0.0, width as f32, height as f32);
        let primitives = primitives
            .into_iter()
            .map(|primitive| ClippedPrimitive {
                clip_rect,
                primitive,
            })
            .collect();
        Painter::new().paint_and_update_textures(surface.canvas(), 1.0, primitives, textures_delta);

        let info = ImageInfo::new(
            (width, height),
            ColorType::RGBA8888,
            AlphaType::Premul,
            None,
        );
        let mut rgba = vec![0; (width * height * 4) as usize];
        assert!(surface.read_pixels(&info, &mut rgba, width as usize * 4, (0, 0)));
        rgba
    }

    #[test]
    fn textures_keep_their_channel_order() {
        let id = TextureId::User(1);
        let texels = vec![Color32::RED, Color32::GREEN, Color32::BLUE];
        let mut delta = TexturesDelta::default();
        delta.push(
            id,
            ImageDelta::full(ColorImage::new([3, 1], texels), TextureOptions::NEAREST),
        );
        let mut mesh = Mesh::with_texture(id);
        mesh.add_rect_with_uv(
            rect(0.0, 0.0, 3.0, 1.0),
            rect(0.0, 0.0, 1.0, 1.0),
            Color32::WHITE,
        );

        let rgba = paint(3, 1, vec![Primitive::Mesh(mesh)], delta);
        let pixels: Vec<&[u8]> = rgba.chunks_exact(4).collect();
        for (i, expected) in [[255, 0, 0], [0, 255, 0], [0, 0, 255]].iter().enumerate() {
            let close = pixels[i][..3]
                .iter()
                .zip(expected)
                .all(|(&got, &want)| got.abs_diff(want) < 8);
            assert!(close, "pixel {i} is {:?}, expected {expected:?}", pixels[i]);
        }
    }

    #[cfg(feature = "cpu_fix")]
    #[test]
    fn font_meshes_past_u16_indices_draw_every_quad() {
        // 12,000 one-pixel quads: 48,000 vertices and 72,000 indices in one mesh.
        let (width, height) = (120, 100);
        let mut delta = TexturesDelta::default();
        delta.push(
            TextureId::default(),
            ImageDelta::full(
                ColorImage::filled([2, 2], Color32::WHITE),
                TextureOptions::NEAREST,
            ),
        );
        let mut mesh = Mesh::with_texture(TextureId::default());
        for y in 0..height {
            for x in 0..width {
                let (x, y) = (x as f32, y as f32);
                mesh.add_rect_with_uv(
                    rect(x, y, x + 1.0, y + 1.0),
                    rect(0.5, 0.5, 1.0, 1.0),
                    Color32::RED,
                );
            }
        }

        let rgba = paint(width, height, vec![Primitive::Mesh(mesh)], delta);
        let unpainted = rgba.chunks_exact(4).filter(|px| px[3] != 255).count();
        assert_eq!(
            unpainted,
            0,
            "{unpainted} of {} pixels unpainted",
            width * height
        );
    }

    #[test]
    fn unusable_items_are_skipped() {
        let mut delta = TexturesDelta::default();
        delta.push(
            TextureId::default(),
            ImageDelta::full(
                ColorImage::filled([2, 2], Color32::WHITE),
                TextureOptions::NEAREST,
            ),
        );
        delta.push(
            TextureId::User(2),
            ImageDelta::full(ColorImage::new([0, 0], vec![]), TextureOptions::NEAREST),
        );
        delta.push(
            TextureId::User(3),
            ImageDelta::partial(
                [0, 0],
                ColorImage::filled([1, 1], Color32::RED),
                TextureOptions::NEAREST,
            ),
        );

        let mut unknown_texture = Mesh::with_texture(TextureId::User(9));
        unknown_texture.add_rect_with_uv(
            rect(0.0, 0.0, 4.0, 4.0),
            rect(0.0, 0.0, 1.0, 1.0),
            Color32::WHITE,
        );

        let vertex = Vertex {
            pos: pos2(1.0, 1.0),
            uv: pos2(0.0, 0.0),
            color: Color32::WHITE,
        };
        let mut no_indices = Mesh::with_texture(TextureId::default());
        no_indices.vertices = vec![vertex; 3];

        let mut out_of_range = Mesh::with_texture(TextureId::default());
        out_of_range.vertices = vec![vertex];
        out_of_range.indices = vec![0, 1, 2];

        let foreign_callback = Primitive::Callback(PaintCallback {
            rect: rect(0.0, 0.0, 4.0, 4.0),
            callback: Arc::new(0_u8),
        });

        let rgba = paint(
            4,
            4,
            vec![
                Primitive::Mesh(unknown_texture),
                Primitive::Mesh(no_indices),
                Primitive::Mesh(out_of_range),
                foreign_callback,
            ],
            delta,
        );
        assert!(rgba.iter().all(|&byte| byte == 0));
    }
}
