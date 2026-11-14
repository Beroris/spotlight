//! The stage's cover leans away from the pointer, as if pressed where it points.
//!
//! Slint draws flat: it can scale and turn a picture, but not lean it back in perspective.
//! So while the cover leans, it's drawn here with OpenGL into a texture that the stage shows in
//! its place. At rest Slint draws the cover itself and none of this runs. The textures are kept
//! while the stage is open, and let go when it closes.

use std::{cell::RefCell, ffi::CStr, num::NonZeroU32, os::raw::c_void};

use glow::HasContext;
use slint::{ComponentHandle, GraphicsAPI, RenderingState};

use crate::AppWindow;

/// How far the cover turns about each axis with the pointer at its edge.
const MOST_DEGREES: f32 = 8.0;
/// How far the eye is from the cover, in cover widths. The nearer, the more the far side shrinks.
const EYE_DISTANCE: f32 = 2.5;
/// How much darker the cover gets for each cover width it leans back, and lighter coming forward.
const SHADE: f32 = 0.4;

const VERTEX: &str = r#"
attribute vec2 corner;
uniform vec2 turn_sin;
uniform vec2 turn_cos;
uniform float art;
uniform float canvas;
uniform float eye;
uniform float reach;
varying vec2 local;
varying float depth;

void main() {
    // A little past the cover, so its edge can fade out over a pixel.
    local = corner * reach;
    vec2 p = local * art;
    // Towards the eye is +z. The right side goes back as the pointer goes right...
    float x = p.x * turn_cos.x;
    float z = -p.x * turn_sin.x;
    // ...and the bottom as it goes down.
    float y = p.y * turn_cos.y + z * turn_sin.y;
    z = -p.y * turn_sin.y + z * turn_cos.y;
    depth = z;
    // w makes the far side smaller, and keeps the picture straight across it.
    gl_Position = vec4(x / (canvas * 0.5), -y / (canvas * 0.5), 0.0, (eye - z) / eye);
}
"#;

const FRAGMENT: &str = r#"
uniform sampler2D cover;
uniform vec2 cover_scale;
uniform float art;
uniform float radius;
uniform float shade;
varying vec2 local;
varying float depth;

void main() {
    // How far outside the rounded corners, in cover pixels.
    vec2 q = abs(local) * art - vec2(art * 0.5 - radius);
    float outside = length(max(q, 0.0)) + min(max(q.x, q.y), 0.0) - radius;
    float coverage = clamp(0.5 - outside, 0.0, 1.0);
    vec4 color = texture2D(cover, vec2(0.5) + local * cover_scale);
    float light = clamp(1.0 + depth * shade, 0.9, 1.04);
    // Slint takes these textures as not premultiplied.
    gl_FragColor = vec4(color.rgb * light, color.a * coverage);
}
"#;

thread_local! {
    static TILT: RefCell<Tilt> = const { RefCell::new(Tilt::Unused) };
}

enum Tilt {
    Unused,
    Ready(Box<Drawer>),
    /// The shaders didn't build, so the cover stays flat.
    Failed,
}

/// Call from the window's rendering notifier, with every state it reports.
pub fn rendering(ui: &AppWindow, state: &RenderingState, api: &GraphicsAPI) {
    TILT.with(|tilt| {
        let mut tilt = tilt.borrow_mut();
        match state {
            RenderingState::BeforeRendering => before_rendering(&mut tilt, ui, api),
            // The context is about to go, with everything made in it.
            RenderingState::RenderingTeardown => {
                if let Tilt::Ready(drawer) = &mut *tilt {
                    unsafe { drawer.let_go(ui) };
                }
                *tilt = Tilt::Unused;
            }
            _ => {}
        }
    });
}

fn before_rendering(tilt: &mut Tilt, ui: &AppWindow, api: &GraphicsAPI) {
    if !ui.get_stage_tilting() {
        if let Tilt::Ready(drawer) = tilt {
            if !ui.get_stage_open() && drawer.holds_textures() {
                unsafe { drawer.let_go(ui) };
            }
        }
        return;
    }
    if matches!(tilt, Tilt::Unused) {
        *tilt = match api {
            GraphicsAPI::NativeOpenGL { get_proc_address } => match unsafe { Drawer::new(get_proc_address) } {
                Ok(drawer) => Tilt::Ready(Box::new(drawer)),
                Err(_) => Tilt::Failed,
            },
            _ => Tilt::Failed,
        };
    }
    let drawn = match tilt {
        Tilt::Ready(drawer) => unsafe { drawer.draw(ui) },
        _ => false,
    };
    if ui.get_stage_tilt_ready() != drawn {
        ui.set_stage_tilt_ready(drawn);
    }
}

struct Drawer {
    gl: glow::Context,
    program: glow::Program,
    corners: glow::Buffer,
    /// What the corners are drawn from, where OpenGL only draws from one.
    vertex_array: Option<glow::VertexArray>,
    uniforms: Uniforms,
    target: Option<Target>,
    cover: Option<Cover>,
}

struct Uniforms {
    turn_sin: Option<glow::UniformLocation>,
    turn_cos: Option<glow::UniformLocation>,
    art: Option<glow::UniformLocation>,
    canvas: Option<glow::UniformLocation>,
    eye: Option<glow::UniformLocation>,
    reach: Option<glow::UniformLocation>,
    cover: Option<glow::UniformLocation>,
    cover_scale: Option<glow::UniformLocation>,
    radius: Option<glow::UniformLocation>,
    shade: Option<glow::UniformLocation>,
}

/// What the leaning cover is drawn into, and what the stage shows.
struct Target {
    framebuffer: glow::Framebuffer,
    texture: glow::Texture,
    size: u32,
}

struct Cover {
    texture: glow::Texture,
    from: slint::Image,
    width: u32,
    height: u32,
}

impl Drawer {
    unsafe fn new(get_proc_address: &dyn Fn(&CStr) -> *const c_void) -> Result<Self, String> {
        unsafe {
            let gl = glow::Context::from_loader_function_cstr(|name| get_proc_address(name));
            let version = gl.version();
            // A Mac gives only OpenGL's core profile. It has newer names for what the shaders
            // use, which these headers give them, and draws only from a vertex array.
            let core = !version.is_embedded
                && (version.major, version.minor) >= (3, 2)
                && gl.get_parameter_i32(glow::CONTEXT_PROFILE_MASK) & glow::CONTEXT_CORE_PROFILE_BIT as i32 != 0;
            let header = |kind: u32| {
                if version.is_embedded {
                    "#version 100\n#ifdef GL_FRAGMENT_PRECISION_HIGH\nprecision highp float;\n#else\nprecision mediump float;\n#endif\n"
                } else if !core {
                    "#version 120\n"
                } else if kind == glow::VERTEX_SHADER {
                    "#version 150\n#define attribute in\n#define varying out\n"
                } else {
                    "#version 150\n#define varying in\n#define texture2D texture\nout vec4 tilt_color;\n#define gl_FragColor tilt_color\n"
                }
            };
            let program = gl.create_program()?;
            for (kind, body) in [(glow::VERTEX_SHADER, VERTEX), (glow::FRAGMENT_SHADER, FRAGMENT)] {
                let shader = gl.create_shader(kind)?;
                gl.shader_source(shader, &format!("{}{body}", header(kind)));
                gl.compile_shader(shader);
                if !gl.get_shader_compile_status(shader) {
                    let log = gl.get_shader_info_log(shader);
                    gl.delete_shader(shader);
                    gl.delete_program(program);
                    return Err(log);
                }
                gl.attach_shader(program, shader);
                // Goes with the program.
                gl.delete_shader(shader);
            }
            gl.bind_attrib_location(program, 0, "corner");
            gl.link_program(program);
            if !gl.get_program_link_status(program) {
                let log = gl.get_program_info_log(program);
                gl.delete_program(program);
                return Err(log);
            }
            let corners = match gl.create_buffer() {
                Ok(buffer) => buffer,
                Err(error) => {
                    gl.delete_program(program);
                    return Err(error);
                }
            };
            let corner_bytes: Vec<u8> =
                [-0.5f32, -0.5, 0.5, -0.5, -0.5, 0.5, 0.5, 0.5].iter().flat_map(|value| value.to_ne_bytes()).collect();
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(corners));
            gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, &corner_bytes, glow::STATIC_DRAW);
            gl.bind_buffer(glow::ARRAY_BUFFER, None);
            let vertex_array = if core {
                match gl.create_vertex_array() {
                    Ok(vertex_array) => Some(vertex_array),
                    Err(error) => {
                        gl.delete_buffer(corners);
                        gl.delete_program(program);
                        return Err(error);
                    }
                }
            } else {
                None
            };
            let at = |name: &str| gl.get_uniform_location(program, name);
            let uniforms = Uniforms {
                turn_sin: at("turn_sin"),
                turn_cos: at("turn_cos"),
                art: at("art"),
                canvas: at("canvas"),
                eye: at("eye"),
                reach: at("reach"),
                cover: at("cover"),
                cover_scale: at("cover_scale"),
                radius: at("radius"),
                shade: at("shade"),
            };
            Ok(Self { gl, program, corners, vertex_array, uniforms, target: None, cover: None })
        }
    }

    fn holds_textures(&self) -> bool {
        self.target.is_some() || self.cover.is_some()
    }

    /// Frees the textures. The program stays, as it's small and the next tilt would build it again.
    unsafe fn let_go(&mut self, ui: &AppWindow) {
        unsafe {
            if let Some(target) = self.target.take() {
                self.gl.delete_framebuffer(target.framebuffer);
                self.gl.delete_texture(target.texture);
            }
            if let Some(cover) = self.cover.take() {
                self.gl.delete_texture(cover.texture);
            }
        }
        ui.set_stage_tilt_image(slint::Image::default());
        ui.set_stage_tilt_ready(false);
    }

    /// Draws the cover as it leans now. False if it couldn't, and Slint should draw it flat.
    unsafe fn draw(&mut self, ui: &AppWindow) -> bool {
        let scale = ui.window().scale_factor();
        let canvas = (ui.get_stage_tilt_canvas() * scale).round();
        let art = ui.get_stage_art_shown() * scale;
        if canvas < 1.0 || art < 1.0 {
            return false;
        }
        unsafe {
            if !self.ensure_cover(&ui.get_album_art()) {
                return false;
            }
            let gl = &self.gl;
            // femtovg sets the rest of what it needs at the start of each frame, but not these.
            let framebuffer = gl.get_parameter_i32(glow::FRAMEBUFFER_BINDING);
            let mut viewport = [0; 4];
            gl.get_parameter_i32_slice(glow::VIEWPORT, &mut viewport);
            let drawn = self.ensure_target(ui, canvas as u32);
            if drawn {
                self.draw_cover(ui, canvas, art);
            }
            let gl = &self.gl;
            gl.bind_framebuffer(glow::FRAMEBUFFER, NonZeroU32::new(framebuffer as u32).map(glow::NativeFramebuffer));
            gl.viewport(viewport[0], viewport[1], viewport[2], viewport[3]);
            drawn
        }
    }

    unsafe fn draw_cover(&self, ui: &AppWindow, canvas: f32, art: f32) {
        let (Some(target), Some(cover)) = (&self.target, &self.cover) else { return };
        let gl = &self.gl;
        let most = MOST_DEGREES.to_radians();
        // About the upright axis as the pointer goes across, and the level one as it goes down.
        let (across, down) = (ui.get_stage_tilt_x() * most, ui.get_stage_tilt_y() * most);
        // The cover's picture fills it as `image-fit: cover` would, zoomed as Slint zooms it.
        let short_side = cover.width.min(cover.height) as f32;
        let zoom = ui.get_stage_art_zoom().max(0.01);
        let cover_scale = (short_side / cover.width as f32 / zoom, short_side / cover.height as f32 / zoom);
        let uniforms = &self.uniforms;
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(target.framebuffer));
            gl.viewport(0, 0, target.size as i32, target.size as i32);
            gl.disable(glow::SCISSOR_TEST);
            gl.disable(glow::STENCIL_TEST);
            gl.disable(glow::DEPTH_TEST);
            gl.disable(glow::CULL_FACE);
            gl.disable(glow::BLEND);
            gl.color_mask(true, true, true, true);
            gl.clear_color(0.0, 0.0, 0.0, 0.0);
            gl.clear(glow::COLOR_BUFFER_BIT);

            gl.use_program(Some(self.program));
            gl.uniform_2_f32(uniforms.turn_sin.as_ref(), across.sin(), down.sin());
            gl.uniform_2_f32(uniforms.turn_cos.as_ref(), across.cos(), down.cos());
            gl.uniform_1_f32(uniforms.art.as_ref(), art);
            gl.uniform_1_f32(uniforms.canvas.as_ref(), canvas);
            gl.uniform_1_f32(uniforms.eye.as_ref(), EYE_DISTANCE * art);
            gl.uniform_1_f32(uniforms.reach.as_ref(), 1.0 + 3.0 / art);
            gl.uniform_1_i32(uniforms.cover.as_ref(), 0);
            gl.uniform_2_f32(uniforms.cover_scale.as_ref(), cover_scale.0, cover_scale.1);
            gl.uniform_1_f32(uniforms.radius.as_ref(), ui.get_stage_art_radius() * ui.window().scale_factor());
            gl.uniform_1_f32(uniforms.shade.as_ref(), SHADE / art);

            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(cover.texture));
            // femtovg has its own vertex array bound, which gets it back afterwards.
            let theirs = self.vertex_array.map(|ours| {
                let theirs = gl.get_parameter_i32(glow::VERTEX_ARRAY_BINDING);
                gl.bind_vertex_array(Some(ours));
                theirs
            });
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(self.corners));
            gl.enable_vertex_attrib_array(0);
            gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, 8, 0);
            gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            gl.disable_vertex_attrib_array(0);
            gl.bind_buffer(glow::ARRAY_BUFFER, None);
            if let Some(theirs) = theirs {
                gl.bind_vertex_array(NonZeroU32::new(theirs as u32).map(glow::NativeVertexArray));
            }
            gl.bind_texture(glow::TEXTURE_2D, None);
            gl.use_program(None);
        }
    }

    /// The cover's picture as a texture, uploaded again only when the song's art changes.
    unsafe fn ensure_cover(&mut self, image: &slint::Image) -> bool {
        if self.cover.as_ref().is_some_and(|cover| cover.from == *image) {
            return true;
        }
        let gl = &self.gl;
        unsafe {
            if let Some(old) = self.cover.take() {
                gl.delete_texture(old.texture);
            }
            let Some(pixels) = image.to_rgba8() else { return false };
            let (width, height) = (pixels.width(), pixels.height());
            if width == 0 || height == 0 {
                return false;
            }
            let Ok(texture) = gl.create_texture() else { return false };
            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            set_smooth_and_clamped(gl);
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::RGBA as i32,
                width as i32,
                height as i32,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(Some(pixels.as_bytes())),
            );
            gl.bind_texture(glow::TEXTURE_2D, None);
            self.cover = Some(Cover { texture, from: image.clone(), width, height });
        }
        true
    }

    /// A texture the size the stage shows it at, made again only when that size changes.
    unsafe fn ensure_target(&mut self, ui: &AppWindow, size: u32) -> bool {
        if self.target.as_ref().is_some_and(|target| target.size == size) {
            return true;
        }
        let gl = &self.gl;
        unsafe {
            if let Some(old) = self.target.take() {
                gl.delete_framebuffer(old.framebuffer);
                gl.delete_texture(old.texture);
            }
            let Ok(texture) = gl.create_texture() else { return false };
            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            set_smooth_and_clamped(gl);
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::RGBA as i32,
                size as i32,
                size as i32,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(None),
            );
            gl.bind_texture(glow::TEXTURE_2D, None);
            let Ok(framebuffer) = gl.create_framebuffer() else {
                gl.delete_texture(texture);
                return false;
            };
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(framebuffer));
            gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(texture), 0);
            if gl.check_framebuffer_status(glow::FRAMEBUFFER) != glow::FRAMEBUFFER_COMPLETE {
                gl.delete_framebuffer(framebuffer);
                gl.delete_texture(texture);
                return false;
            }
            self.target = Some(Target { framebuffer, texture, size });
            // Drawn with GL's rows, bottom first.
            let image = slint::BorrowedOpenGLTextureBuilder::new_gl_2d_rgba_texture(texture.0, [size, size].into())
                .origin(slint::BorrowedOpenGLTextureOrigin::BottomLeft)
                .build();
            ui.set_stage_tilt_image(image);
        }
        true
    }
}

unsafe fn set_smooth_and_clamped(gl: &glow::Context) {
    unsafe {
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);
    }
}
