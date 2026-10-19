use super::*;
use crate::ui::theme;
use windows::core::PCWSTR;

pub(super) struct Brand {
    image: *mut GpBitmap,
    family: *mut GpFontFamily,
    fonts: [*mut GpFont; 2],
    pixels: Vec<u8>,
    text: [Vec<u16>; 2],
    scale: f32,
}
impl Brand {
    pub fn new(screen: &Screen, scale: f32) -> Result<Self> {
        let icon = crate::ui::branding::icon();
        let mut pixels = Vec::with_capacity(icon.rgba.len());
        for c in icon.rgba.chunks_exact(4) {
            let a = u16::from(c[3]);
            pixels.extend([
                ((u16::from(c[2]) * a + 127) / 255) as u8,
                ((u16::from(c[1]) * a + 127) / 255) as u8,
                ((u16::from(c[0]) * a + 127) / 255) as u8,
                c[3],
            ]);
        }
        let scale = scale
            .min(screen.width as f32 / 600.0)
            .min(screen.height as f32 / 110.0)
            .max(0.1);
        let mut brand = Self {
            image: null_mut(),
            family: null_mut(),
            fonts: [null_mut(); 2],
            pixels,
            scale,
            text: [
                crate::APP_NAME.encode_utf16().collect(),
                format!(
                    "v{}  ·  {} × {}  ·  {}% DPI",
                    env!("CARGO_PKG_VERSION"),
                    screen.width,
                    screen.height,
                    screen.dpi_scale.unwrap_or(100)
                )
                .encode_utf16()
                .collect(),
            ],
        };
        unsafe {
            check(GdipCreateBitmapFromScan0(
                icon.width as i32,
                icon.height as i32,
                icon.width as i32 * 4,
                0x000e200b,
                Some(brand.pixels.as_ptr()),
                &mut brand.image,
            ))?;
            check(GdipCreateFontFamilyFromName(
                w!("Segoe UI"),
                null_mut(),
                &mut brand.family,
            ))?;
            check(GdipCreateFont(
                brand.family,
                theme::ANNOTATION_BRAND_TITLE_SIZE * scale,
                FontStyleBold.0,
                UnitPixel,
                &mut brand.fonts[0],
            ))?;
            check(GdipCreateFont(
                brand.family,
                theme::ANNOTATION_BRAND_INFO_SIZE * scale,
                FontStyleRegular.0,
                UnitPixel,
                &mut brand.fonts[1],
            ))?;
        }
        Result::Ok(brand)
    }
    pub fn draw(&self, g: *mut GpGraphics, color: u32, width: u32) -> Result<()> {
        let [_, r, bg, b] = color.to_be_bytes();
        let light = u32::from(r) * 299 + u32::from(bg) * 587 + u32::from(b) * 114 >= 128000;
        let colors = if light {
            theme::ANNOTATION_BRAND_ON_LIGHT
        } else {
            theme::ANNOTATION_BRAND_ON_DARK
        };
        let padding = theme::ANNOTATION_BRAND_PADDING * self.scale;
        let size = theme::ANNOTATION_BRAND_LOGO_SIZE * self.scale;
        unsafe {
            check(GdipDrawImageRect(
                g,
                self.image.cast(),
                padding,
                padding,
                size,
                size,
            ))?;
            check(GdipSetTextRenderingHint(
                g,
                TextRenderingHintAntiAliasGridFit,
            ))?;
            for i in 0..2 {
                let mut objects = StrokeObjects {
                    pen: null_mut(),
                    brush: null_mut(),
                    path: null_mut(),
                };
                check(GdipCreateSolidFill(
                    u32::from_be_bytes([255, colors[i][0], colors[i][1], colors[i][2]]),
                    &mut objects.brush,
                ))?;
                let x = padding + size + theme::ANNOTATION_BRAND_GAP * self.scale;
                let y = padding
                    + if i == 1 {
                        (theme::ANNOTATION_BRAND_TITLE_SIZE + theme::ANNOTATION_BRAND_GAP * 0.5)
                            * self.scale
                    } else {
                        0.0
                    };
                let layout = RectF {
                    X: x,
                    Y: y,
                    Width: (width as f32 - x - padding).max(1.0),
                    Height: 50.0 * self.scale,
                };
                check(GdipDrawString(
                    g,
                    PCWSTR(self.text[i].as_ptr()),
                    self.text[i].len() as i32,
                    self.fonts[i],
                    &layout,
                    std::ptr::null(),
                    objects.brush.cast(),
                ))?;
            }
        }
        Result::Ok(())
    }
}
impl Drop for Brand {
    fn drop(&mut self) {
        unsafe {
            for font in self.fonts {
                if !font.is_null() {
                    GdipDeleteFont(font);
                }
            }
            if !self.family.is_null() {
                GdipDeleteFontFamily(self.family);
            }
            if !self.image.is_null() {
                GdipDisposeImage(self.image.cast());
            }
        }
    }
}
