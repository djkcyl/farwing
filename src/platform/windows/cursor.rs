//! DDA pointer shapes. Only cursor-sized data is uploaded; desktop pixels stay on GPU.
use anyhow::{Context, Result, ensure};
use windows::Win32::{
    Foundation::POINT,
    Graphics::{
        Direct3D11::*,
        Dxgi::{Common::*, *},
    },
};

#[derive(Default)]
pub(super) struct Cursor {
    view: Option<ID3D11ShaderResourceView>,
    position: POINT,
    visible: bool,
    kind: u32,
    width: u32,
    height: u32,
}
impl Cursor {
    pub fn update(
        &mut self,
        device: &ID3D11Device,
        duplication: &IDXGIOutputDuplication,
        frame: &DXGI_OUTDUPL_FRAME_INFO,
    ) -> Result<()> {
        if frame.LastMouseUpdateTime != 0 {
            // DDA supplies the shape's top-left, not GetCursorPos's hotspot.
            self.position = frame.PointerPosition.Position;
            self.visible = frame.PointerPosition.Visible.as_bool();
        }
        if frame.PointerShapeBufferSize == 0 {
            return Ok(());
        }
        ensure!(
            frame.PointerShapeBufferSize <= 4 * 1024 * 1024,
            "光标形状过大"
        );
        let mut raw = vec![0u8; frame.PointerShapeBufferSize as usize];
        let mut info = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
        let mut required = 0;
        unsafe {
            duplication.GetFramePointerShape(
                raw.len() as u32,
                raw.as_mut_ptr().cast(),
                &mut required,
                &mut info,
            )?;
        }
        ensure!(required as usize <= raw.len(), "光标缓冲区不足");
        ensure!(
            matches!(info.Type, 1 | 2 | 4)
                && info.Width > 0
                && info.Width <= 1024
                && info.Height > 0
                && info.Height <= 2048,
            "不支持的光标形状"
        );
        let mono = info.Type == 1;
        ensure!(!mono || info.Height % 2 == 0, "单色光标掩码尺寸无效");
        let height = if mono { info.Height / 2 } else { info.Height };
        ensure!(height <= 1024, "光标高度过大");
        let row = if mono {
            info.Width.div_ceil(8)
        } else {
            info.Width * 4
        };
        ensure!(
            info.Pitch >= row
                && u64::from(info.Pitch) * u64::from(info.Height) <= u64::from(required),
            "光标行跨度无效"
        );
        let mut rgba = vec![0u8; info.Width as usize * height as usize * 4];
        for y in 0..height as usize {
            for x in 0..info.Width as usize {
                let at = (y * info.Width as usize + x) * 4;
                if mono {
                    let bit = 0x80 >> (x % 8);
                    rgba[at] = if raw[y * info.Pitch as usize + x / 8] & bit != 0 {
                        255
                    } else {
                        0
                    };
                    rgba[at + 1] =
                        if raw[(y + height as usize) * info.Pitch as usize + x / 8] & bit != 0 {
                            255
                        } else {
                            0
                        };
                    rgba[at + 3] = 255;
                } else {
                    let source = y * info.Pitch as usize + x * 4;
                    rgba[at..at + 4].copy_from_slice(&[
                        raw[source + 2],
                        raw[source + 1],
                        raw[source],
                        raw[source + 3],
                    ]);
                }
            }
        }
        unsafe {
            let mut texture = None;
            device.CreateTexture2D(
                &D3D11_TEXTURE2D_DESC {
                    Width: info.Width,
                    Height: height,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Usage: D3D11_USAGE_IMMUTABLE,
                    BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                    ..Default::default()
                },
                Some(&D3D11_SUBRESOURCE_DATA {
                    pSysMem: rgba.as_ptr().cast(),
                    SysMemPitch: info.Width * 4,
                    SysMemSlicePitch: 0,
                }),
                Some(&mut texture),
            )?;
            let mut view = None;
            device.CreateShaderResourceView(
                &texture.context("光标纹理未创建")?,
                None,
                Some(&mut view),
            )?;
            self.view = Some(view.context("光标视图未创建")?);
        }
        self.kind = info.Type;
        self.width = info.Width;
        self.height = height;
        Ok(())
    }
    pub fn render(
        &self,
        screen_width: u32,
        screen_height: u32,
    ) -> Option<(ID3D11ShaderResourceView, [f32; 8])> {
        if !self.visible {
            return None;
        }
        Some((
            self.view.as_ref()?.clone(),
            [
                self.position.x as f32,
                self.position.y as f32,
                self.width as f32,
                self.height as f32,
                screen_width as f32,
                screen_height as f32,
                self.kind as f32,
                1.0,
            ],
        ))
    }
}
