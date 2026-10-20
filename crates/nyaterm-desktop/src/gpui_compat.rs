//! Mutable sprite-atlas textures for the remote desktop surface.
//!
//! `gpui-fast` carries the fork's dynamic-texture API, so on Windows the
//! frames RDP/VNC produce are uploaded into a sub-region of an atlas texture
//! (`ID3D11DeviceContext::UpdateSubresource`) and every painted frame reuses
//! that same tile. Nothing is copied per frame beyond the dirty rects the
//! protocol already handed us.
//!
//! The Linux and macOS platform crates still have no in-place atlas upload, so
//! those targets keep a CPU-side buffer whose contents are rebuilt into an
//! image when they change. That path costs one full-buffer copy plus one atlas
//! upload per painted frame, which is why it is limited to the targets that
//! cannot do better.
//!
//! These are free functions rather than a `Window` extension trait on purpose:
//! `Window` now has inherent methods with the same names, and inherent methods
//! win method resolution, which would silently route every call to the engine
//! path even on targets that need the buffered one.

use gpui::{Bounds, DevicePixels, Pixels, Size, Window};

#[cfg(target_os = "windows")]
pub(crate) use gpui::DynamicTexture;

#[cfg(not(target_os = "windows"))]
pub(crate) use buffered::DynamicTexture;

#[cfg(target_os = "windows")]
pub(crate) fn create_dynamic_texture(
    window: &mut Window,
    size: Size<DevicePixels>,
    pixels: &[u8],
    bytes_per_row: u32,
) -> anyhow::Result<DynamicTexture> {
    Window::create_dynamic_texture(window, size, pixels, bytes_per_row)
}

#[cfg(target_os = "windows")]
pub(crate) fn update_dynamic_texture(
    window: &mut Window,
    texture: &DynamicTexture,
    bounds: Bounds<DevicePixels>,
    pixels: &[u8],
    bytes_per_row: u32,
) -> anyhow::Result<()> {
    Window::update_dynamic_texture(window, *texture, bounds, pixels, bytes_per_row)
}

#[cfg(target_os = "windows")]
pub(crate) fn paint_dynamic_texture(
    window: &mut Window,
    bounds: Bounds<Pixels>,
    texture: &DynamicTexture,
) -> anyhow::Result<()> {
    Window::paint_dynamic_texture(window, bounds, *texture)
}

#[cfg(target_os = "windows")]
pub(crate) fn remove_dynamic_texture(window: &mut Window, texture: DynamicTexture) {
    Window::remove_dynamic_texture(window, texture)
}

#[cfg(not(target_os = "windows"))]
pub(crate) use buffered::{
    create_dynamic_texture, paint_dynamic_texture, remove_dynamic_texture, update_dynamic_texture,
};

/// Clones a texture handle.
///
/// `DynamicTexture` is `Copy` on Windows and refcounted on the buffered
/// targets. Spelling `.clone()` at the call site makes clippy flag the `Copy`
/// case, while dropping it breaks the other, so the clone lives here.
#[allow(clippy::clone_on_copy)]
pub(crate) fn clone_texture(texture: &DynamicTexture) -> DynamicTexture {
    texture.clone()
}

/// The vendored engine implements `FileGroupDescriptorW` / `FileContents`
/// streams on Windows, so a remote (SFTP) selection can leave the window
/// without being written to disk first. The other platform crates still ship
/// without it.
pub(crate) fn supports_virtual_file_drag() -> bool {
    cfg!(target_os = "windows")
}

/// Promised drag has the receiver choose the destination path. The Windows
/// shell side is not implemented (a promised payload is refused when the drag
/// starts), and the macOS/Linux crates have no bridge for it either.
pub(crate) fn supports_file_promise_drag() -> bool {
    false
}

/// CPU-side stand-in for platforms whose atlas cannot be written in place.
#[cfg(not(target_os = "windows"))]
mod buffered {
    use std::{cell::RefCell, rc::Rc, sync::Arc};

    use anyhow::{Context as _, ensure};
    use gpui::{Bounds, Corners, DevicePixels, Pixels, Size, Window};

    /// Cloning shares one state cell, which is what lets a paint closure
    /// captured before an update observe the pixels written afterwards.
    #[derive(Clone)]
    pub(crate) struct DynamicTexture(Rc<RefCell<DynamicTextureState>>);

    impl PartialEq for DynamicTexture {
        fn eq(&self, other: &Self) -> bool {
            Rc::ptr_eq(&self.0, &other.0)
        }
    }

    impl std::fmt::Debug for DynamicTexture {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("DynamicTexture").finish_non_exhaustive()
        }
    }

    struct DynamicTextureState {
        width: u32,
        height: u32,
        /// Tightly packed RGBA rows, `width * 4` bytes each.
        pixels: Vec<u8>,
        image: Option<Arc<gpui::RenderImage>>,
        dirty: bool,
    }

    impl DynamicTextureState {
        fn build_image(&self) -> Option<Arc<gpui::RenderImage>> {
            let buffer = image::RgbaImage::from_raw(self.width, self.height, self.pixels.clone())?;
            Some(Arc::new(gpui::RenderImage::new(vec![image::Frame::new(
                buffer,
            )])))
        }

        /// Blits a strided sub-rect into the tightly packed buffer.
        fn blit(
            &mut self,
            x: i32,
            y: i32,
            width: i32,
            height: i32,
            pixels: &[u8],
            stride: usize,
        ) -> anyhow::Result<()> {
            let x = usize::try_from(x.max(0)).unwrap_or(usize::MAX);
            let y = usize::try_from(y.max(0)).unwrap_or(usize::MAX);
            let width = usize::try_from(width.max(0)).unwrap_or(usize::MAX);
            let height = usize::try_from(height.max(0)).unwrap_or(usize::MAX);
            let row_bytes = width
                .checked_mul(4)
                .context("dynamic texture row overflow")?;
            ensure!(stride >= row_bytes, "dynamic texture stride is too small");
            let row_stride = self
                .width
                .checked_mul(4)
                .map(|value| value as usize)
                .context("dynamic texture row overflow")?;
            for row in 0..height {
                let source = row
                    .checked_mul(stride)
                    .context("dynamic texture offset overflow")?;
                let source_row = pixels
                    .get(source..source + row_bytes)
                    .context("dynamic texture update is shorter than the region")?;
                let target_row = y
                    .checked_add(row)
                    .and_then(|line| line.checked_mul(row_stride))
                    .and_then(|line| line.checked_add(x * 4))
                    .context("dynamic texture offset overflow")?;
                let target = self
                    .pixels
                    .get_mut(target_row..target_row + row_bytes)
                    .context("dynamic texture region is outside the texture")?;
                target.copy_from_slice(source_row);
            }
            self.dirty = true;
            Ok(())
        }
    }

    pub(crate) fn create_dynamic_texture(
        window: &mut Window,
        size: Size<DevicePixels>,
        pixels: &[u8],
        bytes_per_row: u32,
    ) -> anyhow::Result<DynamicTexture> {
        let _ = window;
        let width = u32::try_from(size.width.0.max(0)).context("negative texture width")?;
        let height = u32::try_from(size.height.0.max(0)).context("negative texture height")?;
        let mut state = DynamicTextureState {
            width,
            height,
            pixels: vec![0u8; (width as usize) * (height as usize) * 4],
            image: None,
            dirty: true,
        };
        state.blit(
            0,
            0,
            width as i32,
            height as i32,
            pixels,
            bytes_per_row as usize,
        )?;
        Ok(DynamicTexture(Rc::new(RefCell::new(state))))
    }

    pub(crate) fn update_dynamic_texture(
        window: &mut Window,
        texture: &DynamicTexture,
        bounds: Bounds<DevicePixels>,
        pixels: &[u8],
        bytes_per_row: u32,
    ) -> anyhow::Result<()> {
        let _ = window;
        texture.0.borrow_mut().blit(
            bounds.origin.x.0,
            bounds.origin.y.0,
            bounds.size.width.0,
            bounds.size.height.0,
            pixels,
            bytes_per_row as usize,
        )
    }

    pub(crate) fn paint_dynamic_texture(
        window: &mut Window,
        bounds: Bounds<Pixels>,
        texture: &DynamicTexture,
    ) -> anyhow::Result<()> {
        let mut stale = None;
        let image = {
            let mut state = texture.0.borrow_mut();
            if state.image.is_none() || state.dirty {
                // gpui-fast images are immutable, so any update means a new
                // atlas entry; the previous one has to be released by hand.
                let next = state.build_image().context("invalid texture pixels")?;
                stale = state.image.replace(next);
                state.dirty = false;
            }
            state.image.clone()
        };
        if let Some(stale) = stale {
            window.drop_image(stale)?;
        }
        let Some(image) = image else {
            return Ok(());
        };
        window.paint_image(bounds, bounds, Corners::default(), image, 0, false)
    }

    pub(crate) fn remove_dynamic_texture(window: &mut Window, texture: DynamicTexture) {
        let stale = texture.0.borrow_mut().image.take();
        if let Some(stale) = stale {
            window.drop_image(stale).ok();
        }
    }

    #[cfg(test)]
    mod tests {
        use std::{cell::RefCell, rc::Rc};

        use super::{DynamicTexture, DynamicTextureState};

        fn texture(pixels: &[u8], width: u32, height: u32) -> DynamicTexture {
            let mut state = DynamicTextureState {
                width,
                height,
                pixels: vec![0u8; (width as usize) * (height as usize) * 4],
                image: None,
                dirty: true,
            };
            state
                .blit(
                    0,
                    0,
                    width as i32,
                    height as i32,
                    pixels,
                    (width * 4) as usize,
                )
                .expect("blit");
            DynamicTexture(Rc::new(RefCell::new(state)))
        }

        #[test]
        fn blit_copies_a_strided_row_into_the_packed_buffer() {
            let state = DynamicTexture(Rc::new(RefCell::new(DynamicTextureState {
                width: 2,
                height: 2,
                pixels: vec![0u8; 16],
                image: None,
                dirty: false,
            })));
            state
                .0
                .borrow_mut()
                .blit(1, 1, 1, 1, &[1, 2, 3, 4], 4)
                .expect("blit");
            // Row 1 starts at byte 8 and the pixel sits at column 1.
            assert_eq!(&state.0.borrow().pixels[12..16], &[1, 2, 3, 4]);
            assert!(state.0.borrow().dirty);
        }

        #[test]
        fn blit_rejects_a_stride_narrower_than_the_region() {
            let mut state = DynamicTextureState {
                width: 4,
                height: 4,
                pixels: vec![0u8; 64],
                image: None,
                dirty: false,
            };
            assert!(state.blit(0, 0, 4, 1, &[0u8; 8], 8).is_err());
        }

        #[test]
        fn blit_rejects_a_region_outside_the_texture() {
            let mut state = DynamicTextureState {
                width: 2,
                height: 2,
                pixels: vec![0u8; 16],
                image: None,
                dirty: false,
            };
            assert!(state.blit(1, 1, 2, 1, &[0u8; 8], 8).is_err());
        }

        #[test]
        fn texture_exposes_the_pixels_it_was_built_from() {
            let state = texture(&[9, 9, 9, 9, 8, 8, 8, 8], 2, 1);
            assert_eq!(state.0.borrow().pixels, vec![9, 9, 9, 9, 8, 8, 8, 8]);
        }
    }
}
