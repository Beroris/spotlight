# Why this crate is here

This is i-slint-renderer-femtovg 1.16.1 from crates.io with one addition: a backdrop blur, for
the glass player bar. Slint has no way to blur what is behind an element.

How it is asked for: a `Rectangle` whose `drop-shadow-color` is exactly `#5b10b501`. Instead of
a shadow, the renderer copies the part of the window behind that rectangle as drawn so far, blurs
it, and draws it back inside the rectangle's rounded shape. `drop-shadow-blur` is how soft the
blur is, and `border-radius` the shape. Whatever is drawn after it sits on top, sharp.

The changes, which need carrying over when moving to a newer Slint:

- `itemrenderer.rs`: `draw_box_shadow` hands a shadow in that color to the new
  `draw_backdrop_blur`. `BACKDROP_BLUR_MARK` is the color.
- `images.rs`: `TextureImporter::copy_drawn` draws what is queued and copies a piece of the
  window into a texture. Only the OpenGL renderer does it; elsewhere it answers false and
  nothing is drawn.
- `opengl.rs`: keeps an OpenGL handle of its own (`BACKDROP_GL`) for that copy, made where the
  renderer is given its context.

It does nothing inside a layer (an element with `cache-rendering-hint`, an opacity, or a rounded
clip), or on anything turned or scaled, so the rectangle has to be drawn straight to the window.

It costs one extra pass over the bar's area on each frame that is drawn, and ends the frame's
batch of drawing early. Spotlight's setting "Blur behind the player bar" turns it off.

The version stays 1.16.1, so the other Slint crates, which ask for exactly that version, still
accept it.
