//! Video path: hardware H.264 decode (Media Foundation on D3D11), our own
//! shader pass onto a flip swap chain, and the overlay on top. One thread
//! owns the window, the decoder and the renderer; a picture is shown as soon
//! as it is decoded, or a few milliseconds later when smoothing is on.

use crate::input::{self, FrameTiming, InputEvent};
use crate::lifecycle;
use crate::overlay;
use crate::pacing::VideoPacer;
use crate::settings;
use anyhow::{Context, Result, anyhow, bail};
use std::collections::HashMap;
use std::mem::ManuallyDrop;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{HMODULE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
    D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, D3D_SRV_DIMENSION_TEXTURE2DARRAY, ID3DInclude,
};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Media::timeBeginPeriod;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{Interface, PCSTR, s, w};

/// One complete encoded picture in Annex B format.
pub struct AccessUnit {
    pub data: Vec<u8>,
    pub arrival: Instant,
    pub rtp_timestamp: u32,
}

#[derive(Default)]
pub struct VideoStats {
    pub decoded: AtomicU64,
    pub presented: AtomicU64,
    pub skipped: AtomicU64,
    /// Sum of (present time - packet arrival) in microseconds, for presented frames.
    pub latency_us: AtomicU64,
    pub width: AtomicU64,
    pub height: AtomicU64,
    /// Sum over presented frames of |interval to the previous present - one frame time|, microseconds.
    pub cadence_error_us: AtomicU64,
    /// Current adaptive smoothing delay in microseconds.
    pub smoothing_us: AtomicU64,
}

pub struct VideoOptions {
    pub fullscreen: bool,
    /// Save the Nth presented frame as a BMP (used to verify the pipeline).
    pub screenshot: Option<(PathBuf, u64)>,
    /// Per-frame timing reports for the console's stream adaptation.
    pub timings: Option<tokio::sync::mpsc::UnboundedSender<InputEvent>>,
    /// Use our own shader (bicubic upscale, sharpening, exact colour maths).
    /// When false the graphics driver's video processor does the conversion.
    pub enhance: bool,
}

/// Keys the window passes on to the overlay.
fn navigation_key(key: usize) -> Option<overlay::Nav> {
    use overlay::Nav;
    Some(match key {
        0x25 => Nav::Left,
        0x26 => Nav::Up,
        0x27 => Nav::Right,
        0x28 => Nav::Down,
        0x0D | 0x20 => Nav::Accept,
        0x21 => Nav::PreviousTab, // Page Up
        0x09 | 0x22 => Nav::NextTab, // Tab, Page Down
        _ => return None,
    })
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            // Esc opens the menu, and closes it again.
            WM_KEYDOWN if wp.0 == 0x1B => {
                if overlay::wants_input() {
                    overlay::navigate(overlay::Nav::Back);
                } else {
                    overlay::open_menu();
                }
                LRESULT(0)
            }
            WM_KEYDOWN if overlay::wants_input() && navigation_key(wp.0).is_some() => {
                if let Some(nav) = navigation_key(wp.0) {
                    overlay::navigate(nav);
                }
                LRESULT(0)
            }
            // Windows is logging off or shutting down: close the session first.
            WM_QUERYENDSESSION => LRESULT(1),
            WM_ENDSESSION if wp.0 != 0 => {
                lifecycle::request_shutdown_and_wait();
                LRESULT(0)
            }
            // 4 = the machine is about to sleep.
            WM_POWERBROADCAST if wp.0 == 4 => {
                lifecycle::request_suspend_and_wait();
                LRESULT(1)
            }
            // A tap (touch arrives as a left click) opens or operates the overlay.
            WM_LBUTTONDOWN => {
                let x = (lp.0 & 0xffff) as i16 as f32;
                let y = ((lp.0 >> 16) & 0xffff) as i16 as f32;
                let mut client = RECT::default();
                if GetClientRect(hwnd, &mut client).is_ok() {
                    overlay::tap(x, y, client.right as f32, client.bottom as f32);
                }
                LRESULT(0)
            }
            // The pointer is only shown while there is something to click.
            WM_SETCURSOR if (lp.0 & 0xffff) as u32 == HTCLIENT => {
                let arrow = if overlay::wants_input() { LoadCursorW(None, IDC_ARROW).ok() } else { None };
                SetCursor(arrow);
                LRESULT(1)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

/// Tells Windows we draw in real pixels. Without this, on a display with
/// scaling above 100% Windows hands us a smaller virtual screen and stretches
/// our output, which blurs the picture. Must run before anything else in the
/// process touches windows or screen metrics.
pub fn enable_dpi_awareness() {
    unsafe {
        if let Err(e) = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) {
            eprintln!("[video] could not enable per-monitor DPI awareness ({e}); the picture may be scaled by Windows");
        }
    }
}

unsafe fn create_window(fullscreen: bool) -> Result<HWND> {
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: w!("MestreamWindow"),
            ..Default::default()
        };
        if RegisterClassW(&class) == 0 {
            bail!("could not register the window class");
        }
        let (style, x, y, width, height) = if fullscreen {
            (WS_POPUP | WS_VISIBLE, 0, 0, GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN))
        } else {
            let mut r = RECT { left: 0, top: 0, right: 1280, bottom: 720 };
            let style = WS_OVERLAPPEDWINDOW | WS_VISIBLE;
            let _ = AdjustWindowRect(&mut r, style, false);
            (style, CW_USEDEFAULT, CW_USEDEFAULT, r.right - r.left, r.bottom - r.top)
        };
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("MestreamWindow"),
            w!("Mestream"),
            style,
            x,
            y,
            width,
            height,
            None,
            None,
            Some(instance.into()),
            None,
        )?;
        Ok(hwnd)
    }
}

struct Decoder {
    transform: IMFTransform,
    width: u32,
    height: u32,
    sample_time: i64,
}

impl Decoder {
    unsafe fn new(device: &ID3D11Device) -> Result<Self> {
        unsafe {
            MFStartup(MF_VERSION, MFSTARTUP_FULL)?;

            let mut token = 0u32;
            let mut manager = None;
            MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
            let manager = manager.ok_or_else(|| anyhow!("no DXGI device manager"))?;
            manager.ResetDevice(device, token)?;

            let transform: IMFTransform =
                CoCreateInstance(&CLSID_MSH264DecoderMFT, None, CLSCTX_INPROC_SERVER)
                    .context("the Windows H.264 decoder is not available")?;
            let attributes = transform.GetAttributes()?;
            if attributes.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) == 0 {
                bail!("the H.264 decoder does not support D3D11 hardware decoding");
            }
            // Let our shader read the decoded textures directly.
            if let Ok(output) = transform.GetOutputStreamAttributes(0) {
                let flags = D3D11_BIND_DECODER.0 | D3D11_BIND_SHADER_RESOURCE.0;
                let _ = output.SetUINT32(&MF_SA_D3D11_BINDFLAGS, flags as u32);
            }
            // No frame reordering delay: output each picture as soon as it is decoded.
            attributes.SetUINT32(&CODECAPI_AVLowLatencyMode, 1)?;
            let _ = attributes.SetUINT32(&MF_LOW_LATENCY, 1);
            transform
                .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)
                .context("the H.264 decoder rejected the GPU device")?;

            let input = MFCreateMediaType()?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            transform.SetInputType(0, &input, 0)?;

            let mut decoder = Self { transform, width: 0, height: 0, sample_time: 0 };
            decoder.select_output_type()?;
            decoder.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            decoder.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            Ok(decoder)
        }
    }

    unsafe fn select_output_type(&mut self) -> Result<()> {
        unsafe {
            let mut index = 0;
            loop {
                let media_type = self
                    .transform
                    .GetOutputAvailableType(0, index)
                    .context("the H.264 decoder offers no NV12 output")?;
                if media_type.GetGUID(&MF_MT_SUBTYPE)? == MFVideoFormat_NV12 {
                    self.transform.SetOutputType(0, &media_type, 0)?;
                    if let Ok(size) = media_type.GetUINT64(&MF_MT_FRAME_SIZE) {
                        self.width = (size >> 32) as u32;
                        self.height = size as u32;
                    }
                    return Ok(());
                }
                index += 1;
            }
        }
    }

    /// Feeds one access unit and calls `on_frame(texture, array_slice)` for
    /// every decoded picture that comes out.
    unsafe fn decode(
        &mut self,
        data: &[u8],
        mut on_frame: impl FnMut(&ID3D11Texture2D, u32, u32, u32) -> Result<()>,
    ) -> Result<()> {
        unsafe {
            let buffer = MFCreateMemoryBuffer(data.len() as u32)?;
            let mut ptr = std::ptr::null_mut();
            buffer.Lock(&mut ptr, None, None)?;
            std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(data.len() as u32)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(self.sample_time)?;
            self.sample_time += 166_667; // nominal 60 fps, in 100 ns units

            let mut input_pending = true;
            loop {
                if input_pending {
                    match self.transform.ProcessInput(0, &sample, 0) {
                        Ok(()) => input_pending = false,
                        Err(e) if e.code() == MF_E_NOTACCEPTING => {}
                        Err(e) => return Err(e).context("H.264 decoder rejected the frame"),
                    }
                }

                let mut out = [MFT_OUTPUT_DATA_BUFFER::default()];
                let mut status = 0u32;
                let result = self.transform.ProcessOutput(0, &mut out, &mut status);
                let produced = ManuallyDrop::take(&mut out[0].pSample);
                drop(ManuallyDrop::take(&mut out[0].pEvents));
                match result {
                    Ok(()) => {
                        if let Some(produced) = produced {
                            let buffer = produced.GetBufferByIndex(0)?;
                            let dxgi: IMFDXGIBuffer = buffer
                                .cast()
                                .context("decoder output is not a GPU texture (software decode?)")?;
                            let mut raw = std::ptr::null_mut();
                            dxgi.GetResource(&ID3D11Texture2D::IID, &mut raw)?;
                            let texture = ID3D11Texture2D::from_raw(raw);
                            on_frame(&texture, dxgi.GetSubresourceIndex()?, self.width, self.height)?;
                        }
                    }
                    Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                        self.select_output_type()?;
                    }
                    Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
                        if !input_pending {
                            return Ok(());
                        }
                    }
                    Err(e) => return Err(e).context("H.264 decode failed"),
                }
            }
        }
    }
}

struct Renderer {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    swap_chain: IDXGISwapChain1,
    back_buffer: ID3D11Texture2D,
    target_size: (u32, u32),
    processor: Option<Processor>,
    screenshot: Option<(PathBuf, u64)>,
    presented: u64,
    enhance: bool,
    /// The settings as last read; refreshed when their version changes.
    settings: settings::Settings,
    shader: Option<ShaderPass>,
    painter: Option<overlay::Painter>,
    settings_version: u64,
    /// When the first picture was shown; the start-up screen fades out from here.
    first_video_at: Option<Instant>,
}

/// How long the start-up screen takes to dissolve into the picture.
const LOADER_FADE_SECONDS: f32 = 0.7;

/// Our own NV12 -> screen pass (see shader.hlsl).
struct ShaderPass {
    vertex: ID3D11VertexShader,
    pixel: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    constants: ID3D11Buffer,
    target: ID3D11RenderTargetView,
    /// (visible size, texture size) the constants were last written for.
    sizes: Option<((u32, u32), (u32, u32))>,
    views: HashMap<(usize, u32), (ID3D11ShaderResourceView, ID3D11ShaderResourceView)>,
}

unsafe fn compile(source: &str, entry: PCSTR, target: PCSTR) -> Result<Vec<u8>> {
    unsafe {
        let mut code = None;
        let mut errors = None;
        let result = D3DCompile(
            source.as_ptr() as *const _,
            source.len(),
            PCSTR::null(),
            None,
            None::<&ID3DInclude>,
            entry,
            target,
            0,
            0,
            &mut code,
            Some(&mut errors),
        );
        if let Err(e) = result {
            let detail = errors
                .map(|b| {
                    let bytes = std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize());
                    String::from_utf8_lossy(bytes).into_owned()
                })
                .unwrap_or_default();
            bail!("shader compile failed: {e}: {detail}");
        }
        let code = code.ok_or_else(|| anyhow!("shader compiler returned nothing"))?;
        Ok(std::slice::from_raw_parts(code.GetBufferPointer() as *const u8, code.GetBufferSize()).to_vec())
    }
}

struct Processor {
    source_size: (u32, u32),
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    output_view: ID3D11VideoProcessorOutputView,
    input_views: HashMap<(usize, u32), ID3D11VideoProcessorInputView>,
}

impl Renderer {
    unsafe fn new(hwnd: HWND, screenshot: Option<(PathBuf, u64)>, enhance: bool) -> Result<Self> {
        unsafe {
            let mut device = None;
            let mut context = None;
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .context("could not create a D3D11 device")?;
            let device = device.ok_or_else(|| anyhow!("no D3D11 device"))?;
            let context = context.ok_or_else(|| anyhow!("no D3D11 context"))?;
            // The decoder uses the device from its own worker threads.
            let _ = device.cast::<ID3D11Multithread>()?.SetMultithreadProtected(true);

            let dxgi_device: IDXGIDevice1 = device.cast()?;
            dxgi_device.SetMaximumFrameLatency(1)?;
            let factory: IDXGIFactory2 = dxgi_device.GetAdapter()?.GetParent()?;
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                AlphaMode: DXGI_ALPHA_MODE_IGNORE,
                Flags: DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING.0 as u32,
                ..Default::default()
            };
            let swap_chain = factory
                .CreateSwapChainForHwnd(&device, hwnd, &desc, None, None)
                .context("could not create the swap chain")?;
            let _ = factory.MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER);

            let back_buffer: ID3D11Texture2D = swap_chain.GetBuffer(0)?;
            let mut bb = D3D11_TEXTURE2D_DESC::default();
            back_buffer.GetDesc(&mut bb);

            Ok(Self {
                video_device: device.cast()?,
                video_context: context.cast()?,
                device,
                context,
                swap_chain,
                back_buffer,
                target_size: {
                    eprintln!("[video] drawing at {}x{}", bb.Width, bb.Height);
                    overlay::set_surface(bb.Width as f32, bb.Height as f32);
                    (bb.Width, bb.Height)
                },
                processor: None,
                screenshot,
                presented: 0,
                enhance,
                settings: settings::get(),
                shader: None,
                painter: None,
                settings_version: 0,
                first_video_at: None,
            })
        }
    }

    unsafe fn create_processor(&self, source_size: (u32, u32)) -> Result<Processor> {
        unsafe {
            let rate = DXGI_RATIONAL { Numerator: 60, Denominator: 1 };
            let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: rate,
                InputWidth: source_size.0,
                InputHeight: source_size.1,
                OutputFrameRate: rate,
                OutputWidth: self.target_size.0,
                OutputHeight: self.target_size.1,
                Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
            };
            let enumerator = self.video_device.CreateVideoProcessorEnumerator(&content)?;
            let processor = self.video_device.CreateVideoProcessor(&enumerator, 0)?;

            let out_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                ..Default::default()
            };
            let mut output_view = None;
            self.video_device.CreateVideoProcessorOutputView(
                &self.back_buffer,
                &enumerator,
                &out_desc,
                Some(&mut output_view),
            )?;
            let output_view = output_view.ok_or_else(|| anyhow!("no video output view"))?;

            // Source is BT.709 limited range (bit 2: BT.709 matrix, bits 4-5 = 1: 16-235).
            let source_space = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: (1 << 2) | (1 << 4) };
            self.video_context.VideoProcessorSetStreamColorSpace(&processor, 0, &source_space);
            // Output is full-range RGB.
            let target_space = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 };
            self.video_context.VideoProcessorSetOutputColorSpace(&processor, &target_space);
            let source = RECT { left: 0, top: 0, right: source_size.0 as i32, bottom: source_size.1 as i32 };
            self.video_context.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&source));
            self.video_context.VideoProcessorSetStreamFrameFormat(
                &processor,
                0,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            );

            Ok(Processor { source_size, enumerator, processor, output_view, input_views: HashMap::new() })
        }
    }

    unsafe fn create_shader_pass(&self) -> Result<ShaderPass> {
        unsafe {
            let source = include_str!("shader.hlsl");
            let vs_code = compile(source, s!("vs_main"), s!("vs_5_0"))?;
            let ps_code = compile(source, s!("ps_main"), s!("ps_5_0"))?;
            let mut vertex = None;
            self.device.CreateVertexShader(&vs_code, None, Some(&mut vertex))?;
            let mut pixel = None;
            self.device.CreatePixelShader(&ps_code, None, Some(&mut pixel))?;

            let sampler_desc = D3D11_SAMPLER_DESC {
                Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
                AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                MaxLOD: f32::MAX,
                ..Default::default()
            };
            let mut sampler = None;
            self.device.CreateSamplerState(&sampler_desc, Some(&mut sampler))?;

            let buffer_desc = D3D11_BUFFER_DESC {
                ByteWidth: 32,
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                ..Default::default()
            };
            let mut constants = None;
            self.device.CreateBuffer(&buffer_desc, None, Some(&mut constants))?;

            let mut target = None;
            self.device.CreateRenderTargetView(&self.back_buffer, None, Some(&mut target))?;

            Ok(ShaderPass {
                vertex: vertex.ok_or_else(|| anyhow!("no vertex shader"))?,
                pixel: pixel.ok_or_else(|| anyhow!("no pixel shader"))?,
                sampler: sampler.ok_or_else(|| anyhow!("no sampler"))?,
                constants: constants.ok_or_else(|| anyhow!("no constant buffer"))?,
                target: target.ok_or_else(|| anyhow!("no render target"))?,
                sizes: None,
                views: HashMap::new(),
            })
        }
    }

    /// Draws the decoded picture to the back buffer with our own shader.
    unsafe fn draw_enhanced(&mut self, texture: &ID3D11Texture2D, slice: u32, width: u32, height: u32) -> Result<()> {
        unsafe {
            if self.shader.is_none() {
                self.shader = Some(self.create_shader_pass()?);
            }
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            texture.GetDesc(&mut desc);
            let visible = if width > 0 && height > 0 { (width, height) } else { (desc.Width, desc.Height) };
            let sizes = (visible, (desc.Width, desc.Height));
            let picture = self.settings.clone();
            let device = self.device.clone();
            let pass = self.shader.as_mut().unwrap();

            if pass.sizes != Some(sizes) {
                let values: [f32; 8] = [
                    visible.0 as f32,
                    visible.1 as f32,
                    desc.Width as f32,
                    desc.Height as f32,
                    picture.sharpen,
                    picture.saturation,
                    picture.contrast,
                    0.0,
                ];
                self.context.UpdateSubresource(&pass.constants, 0, None, values.as_ptr() as *const _, 0, 0);
                pass.sizes = Some(sizes);
                pass.views.clear();
            }

            // The decoder cycles through a small pool of textures: make the
            // two plane views for each once and keep them.
            let key = (texture.as_raw() as usize, slice);
            if let std::collections::hash_map::Entry::Vacant(slot) = pass.views.entry(key) {
                let plane = |format| -> Result<ID3D11ShaderResourceView> {
                    let mut view_desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
                        Format: format,
                        ViewDimension: D3D_SRV_DIMENSION_TEXTURE2DARRAY,
                        ..Default::default()
                    };
                    view_desc.Anonymous.Texture2DArray = D3D11_TEX2D_ARRAY_SRV {
                        MostDetailedMip: 0,
                        MipLevels: 1,
                        FirstArraySlice: slice,
                        ArraySize: 1,
                    };
                    let mut view = None;
                    device
                        .CreateShaderResourceView(texture, Some(&view_desc), Some(&mut view))
                        .context("the decoder's pictures cannot be read by the shader")?;
                    view.ok_or_else(|| anyhow!("no shader view"))
                };
                slot.insert((plane(DXGI_FORMAT_R8_UNORM)?, plane(DXGI_FORMAT_R8G8_UNORM)?));
            }
            let (luma, chroma) = &pass.views[&key];

            let viewport = D3D11_VIEWPORT {
                Width: self.target_size.0 as f32,
                Height: self.target_size.1 as f32,
                MaxDepth: 1.0,
                ..Default::default()
            };
            let c = &self.context;
            // The settings panel is drawn with Direct2D, which leaves its own state behind.
            c.OMSetBlendState(None, None, u32::MAX);
            c.OMSetDepthStencilState(None, 0);
            c.RSSetState(None);
            c.OMSetRenderTargets(Some(&[Some(pass.target.clone())]), None);
            c.RSSetViewports(Some(&[viewport]));
            c.IASetInputLayout(None);
            c.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            c.VSSetShader(&pass.vertex, None);
            c.PSSetShader(&pass.pixel, None);
            c.PSSetShaderResources(0, Some(&[Some(luma.clone()), Some(chroma.clone())]));
            c.PSSetSamplers(0, Some(&[Some(pass.sampler.clone())]));
            c.PSSetConstantBuffers(0, Some(&[Some(pass.constants.clone())]));
            c.Draw(3, 0);
            // Release the texture so the decoder can write the next picture into it.
            c.PSSetShaderResources(0, Some(&[None, None]));
            Ok(())
        }
    }

    unsafe fn present(&mut self, texture: &ID3D11Texture2D, slice: u32, width: u32, height: u32) -> Result<()> {
        unsafe {
            self.refresh_settings();
            if self.enhance {
                self.draw_enhanced(texture, slice, width, height)?;
            } else {
                self.draw_plain(texture, slice, width, height)?;
            }

            if self.first_video_at.is_none() {
                overlay::end_reconnect();
            }
            let since_first = self.first_video_at.get_or_insert_with(Instant::now).elapsed().as_secs_f32();
            let fade = (since_first / LOADER_FADE_SECONDS).clamp(0.0, 1.0);
            // Ease the loader out so the picture comes up gently.
            let loader = (1.0 - fade) * (1.0 - fade);
            self.paint_overlay(false, loader)?;
            self.finish_frame()
        }
    }

    /// Picks up changes made in the settings menu.
    fn refresh_settings(&mut self) {
        let version = settings::version();
        if version != self.settings_version {
            self.settings_version = version;
            self.settings = settings::get();
            if let Some(pass) = self.shader.as_mut() {
                pass.sizes = None; // rewrite the shader constants
            }
        }
    }

    /// Shows the start-up screen or a prompt while there is no video.
    unsafe fn present_idle(&mut self) -> Result<()> {
        unsafe {
            self.refresh_settings();
            self.paint_overlay(true, 1.0)?;
            self.finish_frame()
        }
    }

    unsafe fn paint_overlay(&mut self, clear: bool, loader: f32) -> Result<()> {
        unsafe {
            if self.painter.is_none() {
                self.painter = Some(overlay::Painter::new(&self.device, &self.back_buffer)?);
            }
            let (w, h) = (self.target_size.0 as f32, self.target_size.1 as f32);
            let scene = overlay::Scene { clear, loader };
            self.painter.as_mut().unwrap().paint(w, h, &scene)
        }
    }

    /// Takes the requested screenshot, if due, and shows the back buffer.
    unsafe fn finish_frame(&mut self) -> Result<()> {
        unsafe {
            self.presented += 1;
            if self.screenshot.as_ref().is_some_and(|(_, at)| self.presented >= *at) {
                let (path, _) = self.screenshot.take().unwrap();
                if let Err(e) = self.save_back_buffer(&path) {
                    eprintln!("[video] screenshot failed: {e:#}");
                }
            }

            if self.settings.vsync {
                self.swap_chain.Present(1, DXGI_PRESENT(0)).ok()?;
            } else {
                // Immediate: lowest latency, tears unless the display runs variable refresh.
                self.swap_chain.Present(0, DXGI_PRESENT_ALLOW_TEARING).ok()?;
            }
            Ok(())
        }
    }

    /// Draws the decoded picture to the back buffer with the driver's video processor.
    unsafe fn draw_plain(&mut self, texture: &ID3D11Texture2D, slice: u32, width: u32, height: u32) -> Result<()> {
        unsafe {
            let source_size = if width > 0 && height > 0 {
                (width, height)
            } else {
                let mut d = D3D11_TEXTURE2D_DESC::default();
                texture.GetDesc(&mut d);
                (d.Width, d.Height)
            };
            if self.processor.as_ref().map(|p| p.source_size) != Some(source_size) {
                self.processor = Some(self.create_processor(source_size)?);
            }
            let video_device = self.video_device.clone();
            let p = self.processor.as_mut().unwrap();

            let key = (texture.as_raw() as usize, slice);
            if !p.input_views.contains_key(&key) {
                let mut desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                    FourCC: 0,
                    ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                    ..Default::default()
                };
                desc.Anonymous.Texture2D = D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: slice };
                let mut view = None;
                video_device.CreateVideoProcessorInputView(texture, &p.enumerator, &desc, Some(&mut view))?;
                p.input_views.insert(key, view.ok_or_else(|| anyhow!("no video input view"))?);
            }

            let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: ManuallyDrop::new(Some(p.input_views[&key].clone())),
                ..Default::default()
            };
            let blt = self.video_context.VideoProcessorBlt(
                &p.processor,
                &p.output_view,
                0,
                std::slice::from_ref(&stream),
            );
            ManuallyDrop::drop(&mut stream.pInputSurface);
            blt.context("video processor blit failed")?;
            Ok(())
        }
    }

    unsafe fn save_back_buffer(&self, path: &PathBuf) -> Result<()> {
        unsafe {
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            self.back_buffer.GetDesc(&mut desc);
            desc.Usage = D3D11_USAGE_STAGING;
            desc.BindFlags = 0;
            desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            desc.MiscFlags = 0;
            let mut staging = None;
            self.device.CreateTexture2D(&desc, None, Some(&mut staging))?;
            let staging = staging.ok_or_else(|| anyhow!("no staging texture"))?;
            self.context.CopyResource(&staging, &self.back_buffer);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;

            let (w, h) = (desc.Width as usize, desc.Height as usize);
            let mut bmp = Vec::with_capacity(54 + w * h * 4);
            bmp.extend_from_slice(b"BM");
            bmp.extend_from_slice(&((54 + w * h * 4) as u32).to_le_bytes());
            bmp.extend_from_slice(&[0; 4]);
            bmp.extend_from_slice(&54u32.to_le_bytes());
            bmp.extend_from_slice(&40u32.to_le_bytes());
            bmp.extend_from_slice(&(w as i32).to_le_bytes());
            bmp.extend_from_slice(&(-(h as i32)).to_le_bytes()); // top-down
            bmp.extend_from_slice(&1u16.to_le_bytes());
            bmp.extend_from_slice(&32u16.to_le_bytes());
            bmp.extend_from_slice(&[0; 24]);
            for row in 0..h {
                let src = (mapped.pData as *const u8).add(row * mapped.RowPitch as usize);
                bmp.extend_from_slice(std::slice::from_raw_parts(src, w * 4));
            }
            self.context.Unmap(&staging, 0);
            std::fs::write(path, bmp)?;
            Ok(())
        }
    }
}

/// Runs the window, decoder and renderer until the window is closed, `quit`
/// is set, or the sender side of `frames` is dropped. Call from a dedicated thread.
pub fn run(
    frames: Receiver<AccessUnit>,
    options: VideoOptions,
    stats: Arc<VideoStats>,
    quit: Arc<AtomicBool>,
) -> Result<()> {
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        let hwnd = create_window(options.fullscreen)?;
        let mut renderer = Renderer::new(hwnd, options.screenshot.clone(), options.enhance)?;
        let mut decoder = Decoder::new(&renderer.device)?;
        let mut queued: Option<AccessUnit> = None;
        let mut sender_gone = false;
        let mut smoothing_ms = f64::from(renderer.settings.smoothing_ms);
        let mut pacer = VideoPacer::new(smoothing_ms);
        let mut last_present: Option<Instant> = None;
        // 1 ms timer resolution so short waits before a present are accurate.
        timeBeginPeriod(1);

        loop {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    return Ok(());
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            let leaving = quit.load(Ordering::Relaxed)
                || sender_gone
                || overlay::quit_requested()
                || lifecycle::SHUTDOWN_REQUESTED.load(Ordering::Relaxed);
            if leaving {
                let _ = DestroyWindow(hwnd);
                quit.store(false, Ordering::Relaxed);
                sender_gone = false;
                continue;
            }

            let unit = match queued.take() {
                Some(unit) => unit,
                None => match frames.recv_timeout(Duration::from_millis(4)) {
                    Ok(unit) => unit,
                    Err(RecvTimeoutError::Timeout) => {
                        if overlay::reconnecting() {
                            pacer = VideoPacer::new(smoothing_ms); // new stream, new clock
                            renderer.first_video_at = None; // fade in again when video returns
                        }
                        if renderer.first_video_at.is_none() || overlay::prompt_shown() {
                            renderer.present_idle()?;
                        }
                        continue;
                    }
                    Err(RecvTimeoutError::Disconnected) => {
                        sender_gone = true;
                        continue;
                    }
                },
            };
            let wanted_smoothing_ms = f64::from(renderer.settings.smoothing_ms);
            if wanted_smoothing_ms != smoothing_ms {
                smoothing_ms = wanted_smoothing_ms;
                pacer = VideoPacer::new(smoothing_ms);
            }
            // If we are behind, decode everything but only show the newest picture.
            queued = frames.try_recv().ok();
            let behind = queued.is_some();
            let present_at_ms = pacer.schedule(input::instant_ms(unit.arrival), unit.rtp_timestamp);
            stats.smoothing_us.store((pacer.delay_ms() * 1000.0) as u64, Ordering::Relaxed);

            decoder.decode(&unit.data, |texture, slice, width, height| {
                stats.decoded.fetch_add(1, Ordering::Relaxed);
                stats.width.store(u64::from(width), Ordering::Relaxed);
                stats.height.store(u64::from(height), Ordering::Relaxed);
                if behind {
                    stats.skipped.fetch_add(1, Ordering::Relaxed);
                    return Ok(());
                }
                let decoded_ms = input::now_ms();
                // Hold the picture until its slot: sleep most of the wait, spin the last millisecond.
                loop {
                    let remaining_ms = present_at_ms - input::now_ms();
                    if remaining_ms <= 0.0 {
                        break;
                    }
                    if remaining_ms > 1.5 {
                        std::thread::sleep(Duration::from_secs_f64((remaining_ms - 1.0) / 1000.0));
                    } else {
                        std::hint::spin_loop();
                    }
                }
                renderer.present(texture, slice, width, height)?;
                stats.presented.fetch_add(1, Ordering::Relaxed);
                let now = Instant::now();
                if let Some(previous) = last_present.replace(now) {
                    let interval_us = now.duration_since(previous).as_micros() as i64;
                    stats.cadence_error_us.fetch_add((interval_us - 16_667).unsigned_abs(), Ordering::Relaxed);
                }
                if let Some(timings) = &options.timings {
                    let arrival_ms = input::instant_ms(unit.arrival) as u32;
                    let _ = timings.send(InputEvent::Frame(FrameTiming {
                        server_data_key: unit.rtp_timestamp,
                        first_packet_arrival_ms: arrival_ms,
                        submitted_ms: arrival_ms,
                        decoded_ms: decoded_ms as u32,
                        rendered_ms: input::now_ms() as u32,
                    }));
                }
                stats
                    .latency_us
                    .fetch_add(unit.arrival.elapsed().as_micros() as u64, Ordering::Relaxed);
                Ok(())
            })?;
        }
    }
}

/// Splits an Annex B byte stream into access units (one picture each).
pub fn split_access_units(data: &[u8]) -> Vec<Vec<u8>> {
    // Find every NAL unit start (position of the 00 00 01 start code).
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            let begin = if i > 0 && data[i - 1] == 0 { i - 1 } else { i };
            starts.push((begin, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }

    let mut units = Vec::new();
    let mut unit_start = starts.first().map(|s| s.0).unwrap_or(0);
    let mut has_picture = false;
    for &(begin, header) in &starts {
        let Some(&nal_header) = data.get(header) else { break };
        let nal_type = nal_header & 0x1f;
        let is_slice = nal_type == 1 || nal_type == 5;
        let first_slice_of_picture = is_slice && data.get(header + 1).is_some_and(|b| b & 0x80 != 0);
        let starts_new_unit = has_picture && (!is_slice || first_slice_of_picture);
        if starts_new_unit {
            units.push(data[unit_start..begin].to_vec());
            unit_start = begin;
            has_picture = false;
        }
        if is_slice {
            has_picture = true;
        }
    }
    if has_picture {
        units.push(data[unit_start..].to_vec());
    }
    units
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_parameter_sets_with_their_picture() {
        let stream = [
            &[0, 0, 0, 1, 0x67, 1][..], // SPS
            &[0, 0, 0, 1, 0x68, 2],     // PPS
            &[0, 0, 0, 1, 0x65, 0x80],  // IDR, first slice
            &[0, 0, 0, 1, 0x41, 0x80],  // P, first slice
            &[0, 0, 0, 1, 0x41, 0x20],  // P, second slice of the same picture
            &[0, 0, 0, 1, 0x67, 1],     // SPS
            &[0, 0, 0, 1, 0x68, 2],     // PPS
            &[0, 0, 0, 1, 0x65, 0x80],  // IDR
        ]
        .concat();
        let units = split_access_units(&stream);
        assert_eq!(units.len(), 3);
        assert_eq!(units[0].len(), 18);
        assert_eq!(units[1].len(), 12);
        assert_eq!(units[2].len(), 18);
        assert_eq!(units.concat(), stream);
    }
}
