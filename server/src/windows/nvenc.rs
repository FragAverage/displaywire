//! Direct NVENC encoding of the converter's NV12 textures, bypassing Media
//! Foundation: ultra-low-latency tuning at preset P1, CBR with a one-frame VBV,
//! an infinite GOP (keyframes only when asked) and no look-ahead, B-frames or
//! reordering. The driver's `nvEncodeAPI64.dll` is loaded at run time.

use std::collections::HashMap;
use std::ffi::c_void;

use moq_nvenc::sys::nvEncodeAPI::*;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW};
use windows::core::{Interface, s, w};

use super::Res;

// `std::mem::zeroed` is invalid for structs holding an enum with no zero variant
// (`frameFieldMode`, `pictureStruct`): set those fields before the struct counts as initialised.
unsafe fn preset_config() -> NV_ENC_PRESET_CONFIG {
    let mut m = std::mem::MaybeUninit::<NV_ENC_PRESET_CONFIG>::zeroed();
    let p = m.as_mut_ptr();
    unsafe {
        std::ptr::addr_of_mut!((*p).presetCfg.frameFieldMode).write(NV_ENC_PARAMS_FRAME_FIELD_MODE::NV_ENC_PARAMS_FRAME_FIELD_MODE_FRAME);
        m.assume_init()
    }
}

unsafe fn pic_params() -> NV_ENC_PIC_PARAMS {
    let mut m = std::mem::MaybeUninit::<NV_ENC_PIC_PARAMS>::zeroed();
    let p = m.as_mut_ptr();
    unsafe {
        std::ptr::addr_of_mut!((*p).pictureStruct).write(NV_ENC_PIC_STRUCT::NV_ENC_PIC_STRUCT_FRAME);
        m.assume_init()
    }
}

unsafe fn lock_bitstream() -> NV_ENC_LOCK_BITSTREAM {
    let mut m = std::mem::MaybeUninit::<NV_ENC_LOCK_BITSTREAM>::zeroed();
    let p = m.as_mut_ptr();
    unsafe {
        std::ptr::addr_of_mut!((*p).pictureStruct).write(NV_ENC_PIC_STRUCT::NV_ENC_PIC_STRUCT_FRAME);
        m.assume_init()
    }
}

type CreateInstance = unsafe extern "C" fn(*mut NV_ENCODE_API_FUNCTION_LIST) -> NVENCSTATUS;

pub struct NvEncoder {
    api: NV_ENCODE_API_FUNCTION_LIST,
    session: *mut c_void,
    output: NV_ENC_OUTPUT_PTR,
    /// Registered input textures by COM pointer: the converter cycles through a few.
    registered: HashMap<usize, NV_ENC_REGISTERED_PTR>,
    width: u32,
    height: u32,
    fps: u32,
    /// Kept (with `init.encodeConfig` pointing at `cfg`) to reconfigure the bitrate later.
    init: Option<Box<NV_ENC_INITIALIZE_PARAMS>>,
    cfg: Option<Box<NV_ENC_CONFIG>>,
    pub name: String,
}

// The session is only used from the encode thread.
unsafe impl Send for NvEncoder {}

macro_rules! call {
    ($self:ident, $f:ident, $what:expr, $($arg:expr),*) => {{
        let f = $self.api.$f.ok_or(concat!("NVENC: no ", stringify!($f)))?;
        let status = f($($arg),*);
        $self.check(status, $what)?;
    }};
}

impl NvEncoder {
    pub fn new(device: &ID3D11Device, width: u32, height: u32, fps: u32, bitrate: u32) -> Res<NvEncoder> {
        unsafe {
            // From System32 only, where the driver puts it: never a planted copy elsewhere on the search path.
            let lib = LoadLibraryExW(w!("nvEncodeAPI64.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32)
                .map_err(|e| format!("nvEncodeAPI64.dll: {e} (NVIDIA driver installed?)"))?;
            let create = GetProcAddress(lib, s!("NvEncodeAPICreateInstance")).ok_or("NvEncodeAPICreateInstance not found")?;
            let create: CreateInstance = std::mem::transmute(create);

            let mut api: NV_ENCODE_API_FUNCTION_LIST = std::mem::zeroed();
            api.version = NV_ENCODE_API_FUNCTION_LIST_VER;
            let status = create(&mut api);
            if status != NVENCSTATUS::NV_ENC_SUCCESS {
                return Err(format!("NvEncodeAPICreateInstance failed: {status:?} (driver too old for this NVENC API?)").into());
            }

            let mut enc = NvEncoder {
                api,
                session: std::ptr::null_mut(),
                output: std::ptr::null_mut(),
                registered: HashMap::new(),
                width,
                height,
                fps,
                init: None,
                cfg: None,
                name: "NVENC (direct)".into(),
            };

            let mut open: NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS = std::mem::zeroed();
            open.version = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER;
            open.deviceType = NV_ENC_DEVICE_TYPE::NV_ENC_DEVICE_TYPE_DIRECTX;
            open.device = device.as_raw();
            open.apiVersion = NVENCAPI_VERSION;
            let mut session = std::ptr::null_mut();
            let f = enc.api.nvEncOpenEncodeSessionEx.ok_or("NVENC: no nvEncOpenEncodeSessionEx")?;
            let status = f(&mut open, &mut session);
            enc.session = session;
            enc.check(status, "open encode session")?;

            // Start from the driver's own ultra-low-latency P1 configuration.
            let codec = NV_ENC_CODEC_HEVC_GUID;
            let preset = NV_ENC_PRESET_P1_GUID;
            let tuning = NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;
            let mut preset_cfg = preset_config();
            preset_cfg.version = NV_ENC_PRESET_CONFIG_VER;
            preset_cfg.presetCfg.version = NV_ENC_CONFIG_VER;
            call!(enc, nvEncGetEncodePresetConfigEx, "get preset config", session, codec, preset, tuning, &mut preset_cfg);

            let mut cfg = Box::new(preset_cfg.presetCfg);
            cfg.version = NV_ENC_CONFIG_VER;
            cfg.profileGUID = NV_ENC_HEVC_PROFILE_MAIN_GUID;
            cfg.gopLength = NVENC_INFINITE_GOPLENGTH;
            cfg.frameIntervalP = 1;
            let rc = &mut cfg.rcParams;
            rc.rateControlMode = NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_CBR;
            rc.averageBitRate = bitrate;
            rc.maxBitRate = bitrate;
            // One frame's worth of bits: no queueing delay from rate control.
            rc.vbvBufferSize = bitrate / fps.max(1);
            rc.vbvInitialDelay = rc.vbvBufferSize;
            rc.multiPass = NV_ENC_MULTI_PASS::NV_ENC_MULTI_PASS_DISABLED;
            rc.lookaheadDepth = 0;
            rc.set_enableLookahead(0);
            rc.set_zeroReorderDelay(1);
            rc.set_enableAQ(0);
            let hevc = &mut cfg.encodeCodecConfig.hevcConfig;
            hevc.idrPeriod = NVENC_INFINITE_GOPLENGTH;
            hevc.set_repeatSPSPPS(1);
            hevc.set_chromaFormatIDC(1);
            hevc.set_outputAUD(0);
            // Same colour description the Media Foundation path declares (BT.709, limited range).
            let vui = &mut hevc.hevcVUIParameters;
            vui.videoSignalTypePresentFlag = 1;
            vui.videoFormat = NV_ENC_VUI_VIDEO_FORMAT::NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED;
            vui.videoFullRangeFlag = 0;
            vui.colourDescriptionPresentFlag = 1;
            vui.colourPrimaries = NV_ENC_VUI_COLOR_PRIMARIES::NV_ENC_VUI_COLOR_PRIMARIES_BT709;
            vui.transferCharacteristics = NV_ENC_VUI_TRANSFER_CHARACTERISTIC::NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709;
            vui.colourMatrix = NV_ENC_VUI_MATRIX_COEFFS::NV_ENC_VUI_MATRIX_COEFFS_BT709;

            let mut init: NV_ENC_INITIALIZE_PARAMS = std::mem::zeroed();
            init.version = NV_ENC_INITIALIZE_PARAMS_VER;
            init.encodeGUID = codec;
            init.presetGUID = preset;
            init.encodeWidth = width;
            init.encodeHeight = height;
            init.darWidth = width;
            init.darHeight = height;
            init.frameRateNum = fps;
            init.frameRateDen = 1;
            init.enableEncodeAsync = 0;
            init.enablePTD = 1;
            init.encodeConfig = &mut *cfg;
            init.maxEncodeWidth = width;
            init.maxEncodeHeight = height;
            init.tuningInfo = tuning;
            init.bufferFormat = NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_NV12;
            call!(enc, nvEncInitializeEncoder, "initialize encoder", session, &mut init);

            let mut out: NV_ENC_CREATE_BITSTREAM_BUFFER = std::mem::zeroed();
            out.version = NV_ENC_CREATE_BITSTREAM_BUFFER_VER;
            call!(enc, nvEncCreateBitstreamBuffer, "create bitstream buffer", session, &mut out);
            enc.output = out.bitstreamBuffer;
            enc.init = Some(Box::new(init));
            enc.cfg = Some(cfg);
            Ok(enc)
        }
    }

    /// Changes the target bitrate of the running encoder, without a keyframe.
    pub fn set_bitrate(&mut self, bps: u32) -> Res<()> {
        let session = self.session;
        let fps = self.fps.max(1);
        let (Some(cfg), Some(init)) = (self.cfg.as_mut(), self.init.as_mut()) else { return Ok(()) };
        let rc = &mut cfg.rcParams;
        rc.averageBitRate = bps;
        rc.maxBitRate = bps;
        rc.vbvBufferSize = bps / fps;
        rc.vbvInitialDelay = rc.vbvBufferSize;
        init.encodeConfig = &mut **cfg;
        unsafe {
            let mut re: NV_ENC_RECONFIGURE_PARAMS = std::mem::zeroed();
            re.version = NV_ENC_RECONFIGURE_PARAMS_VER;
            re.reInitEncodeParams = **init;
            call!(self, nvEncReconfigureEncoder, "reconfigure bitrate", session, &mut re);
        }
        Ok(())
    }

    fn check(&self, status: NVENCSTATUS, what: &str) -> Res<()> {
        if status == NVENCSTATUS::NV_ENC_SUCCESS {
            return Ok(());
        }
        let detail = match (self.api.nvEncGetLastErrorString, self.session.is_null()) {
            (Some(f), false) => {
                let p = unsafe { f(self.session) };
                if p.is_null() { String::new() } else { unsafe { std::ffi::CStr::from_ptr(p) }.to_string_lossy().into_owned() }
            }
            _ => String::new(),
        };
        Err(format!("NVENC {what}: {status:?} {detail}").into())
    }

    fn register(&mut self, tex: &ID3D11Texture2D) -> Res<NV_ENC_REGISTERED_PTR> {
        let key = tex.as_raw() as usize;
        if let Some(r) = self.registered.get(&key) {
            return Ok(*r);
        }
        let mut reg: NV_ENC_REGISTER_RESOURCE = unsafe { std::mem::zeroed() };
        reg.version = NV_ENC_REGISTER_RESOURCE_VER;
        reg.resourceType = NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX;
        reg.width = self.width;
        reg.height = self.height;
        reg.pitch = 0;
        reg.resourceToRegister = tex.as_raw();
        reg.bufferFormat = NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_NV12;
        reg.bufferUsage = NV_ENC_BUFFER_USAGE::NV_ENC_INPUT_IMAGE;
        let session = self.session;
        unsafe { call!(self, nvEncRegisterResource, "register texture", session, &mut reg) };
        self.registered.insert(key, reg.registeredResource);
        Ok(reg.registeredResource)
    }

    /// Encodes one NV12 frame and returns its access unit.
    pub fn encode(&mut self, frame: &ID3D11Texture2D, time_100ns: i64, keyframe: bool) -> Res<Option<Vec<u8>>> {
        let registered = self.register(frame)?;
        let session = self.session;
        unsafe {
            let mut map: NV_ENC_MAP_INPUT_RESOURCE = std::mem::zeroed();
            map.version = NV_ENC_MAP_INPUT_RESOURCE_VER;
            map.registeredResource = registered;
            call!(self, nvEncMapInputResource, "map input", session, &mut map);

            let mut pic = pic_params();
            pic.version = NV_ENC_PIC_PARAMS_VER;
            pic.inputWidth = self.width;
            pic.inputHeight = self.height;
            pic.inputPitch = self.width;
            pic.inputBuffer = map.mappedResource;
            pic.outputBitstream = self.output;
            pic.bufferFmt = map.mappedBufferFmt;
            pic.pictureStruct = NV_ENC_PIC_STRUCT::NV_ENC_PIC_STRUCT_FRAME;
            pic.inputTimeStamp = time_100ns as u64;
            if keyframe {
                pic.encodePicFlags = NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_FORCEIDR as u32 | NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_OUTPUT_SPSPPS as u32;
            }
            let f = self.api.nvEncEncodePicture.ok_or("NVENC: no nvEncEncodePicture")?;
            let status = f(session, &mut pic);
            let encoded = self.check(status, "encode picture");

            let mut data = None;
            if encoded.is_ok() {
                let mut lock = lock_bitstream();
                lock.version = NV_ENC_LOCK_BITSTREAM_VER;
                lock.outputBitstream = self.output;
                lock.set_doNotWait(0);
                call!(self, nvEncLockBitstream, "lock bitstream", session, &mut lock);
                data = Some(std::slice::from_raw_parts(lock.bitstreamBufferPtr.cast::<u8>(), lock.bitstreamSizeInBytes as usize).to_vec());
                call!(self, nvEncUnlockBitstream, "unlock bitstream", session, self.output);
            }
            call!(self, nvEncUnmapInputResource, "unmap input", session, map.mappedResource);
            encoded?;
            Ok(data)
        }
    }
}

impl Drop for NvEncoder {
    fn drop(&mut self) {
        unsafe {
            let session = self.session;
            if session.is_null() {
                return;
            }
            for (_, r) in self.registered.drain() {
                if let Some(f) = self.api.nvEncUnregisterResource {
                    f(session, r);
                }
            }
            if !self.output.is_null()
                && let Some(f) = self.api.nvEncDestroyBitstreamBuffer
            {
                f(session, self.output);
            }
            if let Some(f) = self.api.nvEncDestroyEncoder {
                f(session);
            }
        }
    }
}
