//! Hardware HEVC encoding through the GPU vendor's Media Foundation transform
//! (NVENC on NVIDIA). Low-latency mode, CBR, no B-frames, long GOP with
//! keyframes on demand.

use std::collections::VecDeque;
use std::mem::ManuallyDrop;

use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncCommonRateControlMode, CODECAPI_AVEncMPVDefaultBPictureCount,
    CODECAPI_AVEncMPVGOPSize, CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVEncVideoMaxKeyframeDistance, CODECAPI_AVLowLatencyMode, ICodecAPI, IMF2DBuffer,
    IMFActivate, IMFDXGIDeviceManager, IMFMediaEventGenerator, IMFMediaType, IMFTransform,
    METransformHaveOutput, METransformNeedInput, MF_E_NO_EVENTS_AVAILABLE,
    MF_E_TRANSFORM_STREAM_CHANGE, MF_EVENT_FLAG_NO_WAIT, MF_LOW_LATENCY, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE,
    MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_MPEG_SEQUENCE_HEADER, MF_MT_MPEG2_PROFILE,
    MF_MT_SUBTYPE, MF_MT_TRANSFER_FUNCTION, MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_VIDEO_PRIMARIES, MF_MT_YUV_MATRIX,
    MF_TRANSFORM_ASYNC, MF_TRANSFORM_ASYNC_UNLOCK, MFCreateDXGIDeviceManager, MFCreateDXGISurfaceBuffer,
    MFCreateMediaType, MFCreateSample, MFMediaType_Video, MFNominalRange_16_235, MFSTARTUP_FULL, MFStartup,
    MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_ADAPTER_LUID, MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER, MFT_FRIENDLY_NAME_Attribute,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_MESSAGE_SET_D3D_MANAGER,
    MFT_OUTPUT_DATA_BUFFER, MFT_REGISTER_TYPE_INFO, MFTEnumEx, MFVideoFormat_HEVC, MFVideoFormat_NV12,
    MFVideoInterlace_Progressive, MFVideoPrimaries_BT709, MFVideoTransFunc_709, MFVideoTransferMatrix_BT709,
    MF_VERSION,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Variant::VARIANT;
use windows::core::{GUID, Interface, PWSTR};

use super::{Context, Res};

/// eAVEncH265VProfile_Main_420_8
const HEVC_PROFILE_MAIN: u32 = 1;
/// eAVEncCommonRateControlMode_CBR
const RATE_CONTROL_CBR: u32 = 0;
/// How long to wait for the encoder to return a frame before moving on.
const OUTPUT_WAIT_NS: u64 = 50_000_000;

pub struct Encoder {
    transform: IMFTransform,
    events: IMFMediaEventGenerator,
    codec: Option<ICodecAPI>,
    _manager: IMFDXGIDeviceManager,
    need_input: u32,
    outputs: VecDeque<Vec<u8>>,
    /// VPS/SPS/PPS from the output type, for encoders that keep them out of band.
    sequence_header: Vec<u8>,
    pub name: String,
}

fn set_codec(codec: &ICodecAPI, api: &GUID, value: impl Into<VARIANT>, what: &str) {
    if let Err(e) = unsafe { codec.SetValue(api, &value.into()) } {
        println!("encoder: {what} not supported ({e})");
    }
}

impl Encoder {
    pub fn new(device: &ID3D11Device, width: u32, height: u32, fps: u32, bitrate: u32) -> Res<Encoder> {
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }.ctx("MFStartup")?;
        let adapter = unsafe { device.cast::<IDXGIDevice>()?.GetAdapter()?.GetDesc()? }.AdapterLuid;
        let (transform, name) = find_encoder(((adapter.HighPart as i64) << 32) | adapter.LowPart as i64)?;
        println!("encoder: {name}");

        let attrs = unsafe { transform.GetAttributes()? };
        unsafe {
            if attrs.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) == 0 {
                return Err(format!("{name} is not an asynchronous (hardware) MFT").into());
            }
            attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
            attrs.SetUINT32(&MF_LOW_LATENCY, 1)?;
        }

        let mut token = 0;
        let mut manager = None;
        unsafe { MFCreateDXGIDeviceManager(&mut token, &mut manager)? };
        let manager = manager.ok_or("no DXGI device manager")?;
        unsafe {
            manager.ResetDevice(device, token).ctx("ResetDevice")?;
            transform
                .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)
                .ctx("encoder: set D3D manager")?;
        }

        let size = ((width as u64) << 32) | height as u64;
        let rate = ((fps as u64) << 32) | 1;
        unsafe {
            let out = MFCreateMediaType()?;
            out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_HEVC)?;
            out.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
            out.SetUINT64(&MF_MT_FRAME_SIZE, size)?;
            out.SetUINT64(&MF_MT_FRAME_RATE, rate)?;
            out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            out.SetUINT32(&MF_MT_MPEG2_PROFILE, HEVC_PROFILE_MAIN)?;
            out.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)?;
            out.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
            out.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32)?;
            out.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
            transform.SetOutputType(0, &out, 0).ctx("encoder: SetOutputType (HEVC)")?;

            let input = MFCreateMediaType()?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            input.SetUINT64(&MF_MT_FRAME_SIZE, size)?;
            input.SetUINT64(&MF_MT_FRAME_RATE, rate)?;
            input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            transform.SetInputType(0, &input, 0).ctx("encoder: SetInputType (NV12)")?;
        }

        let codec = transform.cast::<ICodecAPI>().ok();
        if let Some(c) = &codec {
            set_codec(c, &CODECAPI_AVLowLatencyMode, true, "low latency mode");
            set_codec(c, &CODECAPI_AVEncCommonRateControlMode, RATE_CONTROL_CBR, "CBR");
            set_codec(c, &CODECAPI_AVEncCommonMeanBitRate, bitrate, "bitrate");
            set_codec(c, &CODECAPI_AVEncMPVDefaultBPictureCount, 0, "no B-frames");
            // Long GOP: keyframes come from client requests, not a schedule.
            set_codec(c, &CODECAPI_AVEncMPVGOPSize, fps * 60, "GOP size");
            set_codec(c, &CODECAPI_AVEncVideoMaxKeyframeDistance, fps * 60, "max keyframe distance");
        } else {
            println!("encoder: no ICodecAPI, using defaults");
        }

        let events: IMFMediaEventGenerator = transform.cast()?;
        unsafe {
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0).ctx("encoder: begin streaming")?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0).ctx("encoder: start of stream")?;
        }
        let mut enc = Encoder {
            transform,
            events,
            codec,
            _manager: manager,
            need_input: 0,
            outputs: VecDeque::new(),
            sequence_header: Vec::new(),
            name,
        };
        enc.read_sequence_header();
        Ok(enc)
    }

    fn read_sequence_header(&mut self) {
        let blob = unsafe { self.transform.GetOutputCurrentType(0) }.ok().and_then(|t: IMFMediaType| {
            let mut ptr = std::ptr::null_mut();
            let mut len = 0u32;
            unsafe { t.GetAllocatedBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut ptr, &mut len) }.ok()?;
            let v = unsafe { std::slice::from_raw_parts(ptr, len as usize) }.to_vec();
            unsafe { CoTaskMemFree(Some(ptr as *const _)) };
            Some(v)
        });
        self.sequence_header = blob.unwrap_or_default();
    }

    /// Handles one pending event. Returns false if none was waiting.
    fn pump(&mut self) -> Res<bool> {
        let event = match unsafe { self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
            Ok(e) => e,
            Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(false),
            Err(e) => return Err(e).ctx("encoder: GetEvent"),
        };
        let ty = unsafe { event.GetType()? };
        if ty == METransformNeedInput.0 as u32 {
            self.need_input += 1;
        } else if ty == METransformHaveOutput.0 as u32
            && let Some(data) = self.take_output()?
        {
            self.outputs.push_back(data);
        }
        Ok(true)
    }

    fn take_output(&mut self) -> Res<Option<Vec<u8>>> {
        let mut buffers = [MFT_OUTPUT_DATA_BUFFER::default()];
        let mut status = 0;
        let r = unsafe { self.transform.ProcessOutput(0, &mut buffers, &mut status) };
        let [mut out] = buffers;
        let sample = unsafe { ManuallyDrop::take(&mut out.pSample) };
        unsafe { ManuallyDrop::drop(&mut out.pEvents) };
        match r {
            Ok(()) => {}
            Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                let t = unsafe { self.transform.GetOutputAvailableType(0, 0)? };
                unsafe { self.transform.SetOutputType(0, &t, 0)? };
                self.read_sequence_header();
                return Ok(None);
            }
            Err(e) => return Err(e).ctx("encoder: ProcessOutput"),
        }
        let Some(sample) = sample else { return Ok(None) };
        let buffer = unsafe { sample.ConvertToContiguousBuffer()? };
        let mut ptr = std::ptr::null_mut();
        let mut len = 0u32;
        unsafe { buffer.Lock(&mut ptr, None, Some(&mut len))? };
        let mut data = unsafe { std::slice::from_raw_parts(ptr, len as usize) }.to_vec();
        unsafe { buffer.Unlock()? };
        if hevc::is_keyframe(&data)
            && !hevc::nals(&data).iter().any(|n| hevc::nal_type(n) == hevc::VPS)
            && !self.sequence_header.is_empty()
        {
            data.splice(0..0, self.sequence_header.iter().copied());
        }
        Ok(Some(data))
    }

    /// Changes the target bitrate of the running encoder.
    pub fn set_bitrate(&mut self, bps: u32) {
        if let Some(c) = &self.codec {
            set_codec(c, &CODECAPI_AVEncCommonMeanBitRate, bps, "bitrate");
        }
    }

    /// Encodes one NV12 frame and returns its access unit, or `None` if the
    /// encoder hasn't produced it within `OUTPUT_WAIT_NS` (it'll come out of a later call).
    pub fn encode(&mut self, frame: &ID3D11Texture2D, time_100ns: i64, keyframe: bool) -> Res<Option<Vec<u8>>> {
        while self.need_input == 0 {
            if !self.pump()? {
                std::thread::yield_now();
            }
        }
        if keyframe && let Some(c) = &self.codec {
            set_codec(c, &CODECAPI_AVEncVideoForceKeyFrame, 1u32, "force keyframe");
        }
        unsafe {
            let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, frame, 0, false)?;
            let len = buffer.cast::<IMF2DBuffer>()?.GetContiguousLength()?;
            buffer.SetCurrentLength(len)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(time_100ns)?;
            sample.SetSampleDuration(0)?;
            self.transform.ProcessInput(0, &sample, 0).ctx("encoder: ProcessInput")?;
        }
        self.need_input -= 1;

        let deadline = timing::now_ns() + OUTPUT_WAIT_NS;
        while self.outputs.is_empty() && timing::now_ns() < deadline {
            if !self.pump()? {
                std::thread::yield_now();
            }
        }
        Ok(self.outputs.pop_front())
    }
}

/// Hardware HEVC encoders, those on `adapter` first, activated in turn until one works.
fn find_encoder(adapter: i64) -> Res<(IMFTransform, String)> {
    let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
    let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_HEVC };
    let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input),
            Some(&output),
            &mut list,
            &mut count,
        )
        .ctx("MFTEnumEx")?;
    }
    if count == 0 || list.is_null() {
        return Err("no hardware HEVC encoder found (is the GPU driver installed?)".into());
    }
    let mut candidates: Vec<(IMFActivate, String, bool)> = unsafe { std::slice::from_raw_parts_mut(list, count as usize) }
        .iter_mut()
        .filter_map(Option::take)
        .map(|activate| {
            let mut name = PWSTR::null();
            let mut len = 0;
            let label = unsafe { activate.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut name, &mut len) }
                .ok()
                .map(|_| {
                    let s = unsafe { name.to_string() }.unwrap_or_default();
                    unsafe { CoTaskMemFree(Some(name.0 as *const _)) };
                    s
                })
                .unwrap_or_else(|| "hardware HEVC encoder".into());
            let luid = unsafe { activate.GetUINT64(&MFT_ENUM_ADAPTER_LUID) }.ok();
            let same_gpu = luid.is_some_and(|l| l as i64 == adapter);
            (activate, label, same_gpu)
        })
        .collect();
    unsafe { CoTaskMemFree(Some(list as *const _)) };
    candidates.sort_by_key(|c| !c.2);

    let mut errors = Vec::new();
    for (activate, label, _) in candidates {
        match unsafe { activate.ActivateObject::<IMFTransform>() } {
            Ok(t) => return Ok((t, label)),
            Err(e) => {
                println!("encoder: {label} unavailable: {} (0x{:08X})", e.message().trim(), e.code().0 as u32);
                errors.push(label);
            }
        }
    }
    Err(format!("no hardware HEVC encoder could be started (tried {})", errors.join(", ")).into())
}
