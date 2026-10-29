//! BGRA → NV12 (BT.709, limited range) on the GPU's fixed-function video processor.

use std::mem::ManuallyDrop;

use windows::Win32::Foundation::{RECT, TRUE};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
    D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D, ID3D11Device, ID3D11DeviceContext,
    ID3D11Texture2D, ID3D11VideoContext1, ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
    ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709, DXGI_FORMAT_NV12,
    DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::core::Interface;

use super::{Context, Res};

/// Number of NV12 targets cycled through, so the encoder can still be reading
/// one while the next is written.
const RING: usize = 3;

pub struct Converter {
    video_context: ID3D11VideoContext1,
    processor: ID3D11VideoProcessor,
    input: ID3D11VideoProcessorInputView,
    outputs: Vec<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)>,
    next: usize,
}

impl Converter {
    /// Converts from `source` (fixed BGRA texture of `in_w`×`in_h`) into NV12 of `out_w`×`out_h`.
    pub fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        source: &ID3D11Texture2D,
        (in_w, in_h): (u32, u32),
        (out_w, out_h): (u32, u32),
        fps: u32,
    ) -> Res<Converter> {
        let video_device: ID3D11VideoDevice = device.cast()?;
        let video_context: ID3D11VideoContext1 = context.cast()?;
        let rate = DXGI_RATIONAL { Numerator: fps, Denominator: 1 };
        let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: in_w,
            InputHeight: in_h,
            OutputFrameRate: rate,
            OutputWidth: out_w,
            OutputHeight: out_h,
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
        };
        let enumerator: ID3D11VideoProcessorEnumerator = unsafe { video_device.CreateVideoProcessorEnumerator(&content) }.ctx("CreateVideoProcessorEnumerator")?;
        let processor = unsafe { video_device.CreateVideoProcessor(&enumerator, 0) }.ctx("CreateVideoProcessor")?;

        let in_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 } },
        };
        let mut input = None;
        unsafe { video_device.CreateVideoProcessorInputView(source, &enumerator, &in_desc, Some(&mut input)) }
            .ctx("CreateVideoProcessorInputView")?;
        let input = input.ok_or("no input view")?;

        let out_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
        };
        let mut outputs = Vec::with_capacity(RING);
        for _ in 0..RING {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: out_w,
                Height: out_h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_NV12,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut tex = None;
            unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex)) }.ctx("create NV12 texture")?;
            let tex = tex.ok_or("no NV12 texture")?;
            let mut view = None;
            unsafe { video_device.CreateVideoProcessorOutputView(&tex, &enumerator, &out_desc, Some(&mut view)) }
                .ctx("CreateVideoProcessorOutputView")?;
            outputs.push((tex, view.ok_or("no output view")?));
        }

        unsafe {
            video_context.VideoProcessorSetStreamFrameFormat(&processor, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            video_context.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            video_context.VideoProcessorSetStreamColorSpace1(&processor, 0, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
            video_context.VideoProcessorSetOutputColorSpace1(&processor, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709);
            let src = RECT { left: 0, top: 0, right: in_w as i32, bottom: in_h as i32 };
            video_context.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&src));
            let dst = RECT { left: 0, top: 0, right: out_w as i32, bottom: out_h as i32 };
            video_context.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&dst));
        }
        println!("converter ready");
        Ok(Converter { video_context, processor, input, outputs, next: 0 })
    }

    /// Converts the source texture into the next NV12 target and returns it.
    pub fn convert(&mut self) -> Res<ID3D11Texture2D> {
        let (tex, view) = &self.outputs[self.next];
        self.next = (self.next + 1) % RING;
        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: TRUE,
            pInputSurface: ManuallyDrop::new(Some(self.input.clone())),
            ..Default::default()
        };
        let streams = [stream];
        let r = unsafe { self.video_context.VideoProcessorBlt(&self.processor, view, 0, &streams) };
        let [mut stream] = streams;
        unsafe { ManuallyDrop::drop(&mut stream.pInputSurface) };
        r.ctx("VideoProcessorBlt")?;
        Ok(tex.clone())
    }
}
