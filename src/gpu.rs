//! D3D11 渲染：每张图上传一次纹理，每帧只更新几个 uniform，缩放交给 GPU 采样。

use std::fmt;
use std::sync::{Mutex, OnceLock};

use windows::core::PCSTR;
use windows::Win32::Foundation::{DXGI_STATUS_OCCLUDED, FALSE, HWND, TRUE};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{ID3DBlob, D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_11_0,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDeviceAndSwapChain, ID3D11BlendState, ID3D11Buffer, ID3D11Device,
    ID3D11DeviceContext, ID3D11PixelShader, ID3D11RasterizerState, ID3D11RenderTargetView,
    ID3D11SamplerState, ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11VertexShader,
    D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
    D3D11_BLEND_DESC, D3D11_BLEND_INV_SRC_ALPHA, D3D11_BLEND_OP_ADD, D3D11_BLEND_SRC_ALPHA,
    D3D11_BUFFER_DESC, D3D11_COLOR_WRITE_ENABLE_ALL, D3D11_COMPARISON_ALWAYS, D3D11_CULL_NONE,
    D3D11_FILL_SOLID, D3D11_FILTER_MIN_MAG_MIP_LINEAR, D3D11_RASTERIZER_DESC,
    D3D11_RENDER_TARGET_BLEND_DESC, D3D11_SAMPLER_DESC, D3D11_SDK_VERSION, D3D11_SUBRESOURCE_DATA,
    D3D11_TEXTURE2D_DESC, D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_USAGE_DEFAULT, D3D11_USAGE_IMMUTABLE,
    D3D11_VIEWPORT as D3D11_VIEWPORT_,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_UNKNOWN, DXGI_MODE_DESC, DXGI_MODE_SCALING_UNSPECIFIED,
    DXGI_MODE_SCANLINE_ORDER_UNSPECIFIED, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGISwapChain, DXGI_ERROR_DEVICE_HUNG, DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET,
    DXGI_ERROR_DRIVER_INTERNAL_ERROR, DXGI_SWAP_CHAIN_DESC, DXGI_SWAP_CHAIN_FLAG,
    DXGI_SWAP_EFFECT_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT,
};

use crate::render::Frame;

const SHADER: &str = r#"
cbuffer Params : register(b0)
{
    float2 screenSize;
    float2 bgOffset;
    float4 posterRect;
    float4 posterUV;
    float4 colorAlpha;
};

Texture2D bgTex : register(t0);
Texture2D posterTex : register(t1);
SamplerState samp : register(s0);

struct VSOut { float4 pos : SV_Position; };

VSOut VS(uint id : SV_VertexID)
{
    float2 p = float2(id == 1 ? 3.0 : -1.0, id == 2 ? 3.0 : -1.0);
    VSOut o;
    o.pos = float4(p, 0.0, 1.0);
    return o;
}

float4 PS(VSOut i) : SV_Target
{
    float2 uv = (i.pos.xy - bgOffset) / screenSize;
    if (uv.x < 0.0 || uv.x > 1.0 || uv.y < 0.0 || uv.y > 1.0)
    {
        // 海报墙整张画布滑入时，还没覆盖到的地方保留旧场景。
        return float4(0.0, 0.0, 0.0, 0.0);
    }

    float3 c = bgTex.SampleLevel(samp, uv, 0).rgb;
    float alpha = colorAlpha.w;
    if (posterRect.z > 0.0 && posterRect.w > 0.0)
    {
        float2 q = (i.pos.xy - posterRect.xy) / posterRect.zw;
        if (q.x >= 0.0 && q.x <= 1.0 && q.y >= 0.0 && q.y <= 1.0)
        {
            float2 puv = posterUV.xy + q * posterUV.zw;
            c = posterTex.SampleLevel(samp, puv, 0).rgb;
            alpha = colorAlpha.x;
        }
    }
    return float4(c, alpha);
}
"#;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Params {
    screen: [f32; 2],
    bg_offset: [f32; 2],
    rect: [f32; 4],
    uv: [f32; 4],
    /// x = 海报层透明度，w = 背景/整屏画布透明度。
    color_alpha: [f32; 4],
}

/// GPU 操作错误。保留 HRESULT 与设备移除原因，供上层决定是否重建。
#[derive(Debug, Clone)]
pub struct GpuError {
    context: &'static str,
    hresult: i32,
    device_lost: bool,
    message: String,
}

impl GpuError {
    fn from_win(context: &'static str, err: windows::core::Error) -> Self {
        let hresult = err.code().0;
        Self {
            context,
            hresult,
            device_lost: is_device_loss(hresult),
            message: err.message(),
        }
    }

    fn text(context: &'static str, message: impl Into<String>) -> Self {
        Self {
            context,
            hresult: 0,
            device_lost: false,
            message: message.into(),
        }
    }

    /// 是否属于必须重建 D3D 设备的故障。
    pub fn device_lost(&self) -> bool {
        self.device_lost
    }
}

impl fmt::Display for GpuError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.hresult == 0 {
            write!(f, "{}: {}", self.context, self.message)
        } else {
            write!(
                f,
                "{}: {}（HRESULT 0x{:08X}{}）",
                self.context,
                self.message,
                self.hresult as u32,
                if self.device_lost {
                    "，设备已移除"
                } else {
                    ""
                }
            )
        }
    }
}

/// `Present` 的成功状态。遮挡不是故障，不能因此丢弃设备。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresentStatus {
    Presented,
    Occluded,
}

/// DXGI 设备级故障码；其余 HRESULT 视为普通/临时错误。
fn is_device_loss(hresult: i32) -> bool {
    [
        DXGI_ERROR_DEVICE_REMOVED.0,
        DXGI_ERROR_DEVICE_HUNG.0,
        DXGI_ERROR_DEVICE_RESET.0,
        DXGI_ERROR_DRIVER_INTERNAL_ERROR.0,
    ]
    .contains(&hresult)
}

#[derive(Clone)]
struct ShaderBytes {
    vs: Vec<u8>,
    ps: Vec<u8>,
}

static SHADER_CACHE: OnceLock<Mutex<Option<ShaderBytes>>> = OnceLock::new();

/// 编译一次并缓存字节码；重建设备时直接复用。
fn shader_bytes() -> Result<ShaderBytes, GpuError> {
    let slot = SHADER_CACHE.get_or_init(|| Mutex::new(None));
    cache_or_build(slot, || {
        let vs = compile(SHADER, "VS", "vs_4_0")?;
        let ps = compile(SHADER, "PS", "ps_4_0")?;
        Ok(ShaderBytes {
            vs: blob_bytes(&vs).to_vec(),
            ps: blob_bytes(&ps).to_vec(),
        })
    })
}

/// 只在成功时写入缓存；编译失败要允许下一次重试。
fn cache_or_build<T: Clone, E>(
    slot: &Mutex<Option<T>>,
    build: impl FnOnce() -> Result<T, E>,
) -> Result<T, E> {
    {
        let guard = slot.lock().unwrap_or_else(|poison| poison.into_inner());
        if let Some(value) = guard.as_ref() {
            return Ok(value.clone());
        }
    }
    let value = build()?;
    let mut guard = slot.lock().unwrap_or_else(|poison| poison.into_inner());
    *guard = Some(value.clone());
    Ok(value)
}

pub struct Textures {
    bg: ID3D11ShaderResourceView,
    /// 海报墙布局没有独立海报层，绘制时绑定 1×1 占位纹理。
    poster: Option<ID3D11ShaderResourceView>,
    rect: [f32; 4],
    wall: bool,
}

impl Textures {
    /// 海报在屏幕上的矩形：[x, y, 宽, 高]；海报墙返回整屏矩形。
    pub fn rect(&self) -> [f32; 4] {
        self.rect
    }

    /// 是否为整屏合成画布（海报墙）。
    pub fn is_wall(&self) -> bool {
        self.wall
    }
}

pub struct Gpu {
    swap: IDXGISwapChain,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    cb: ID3D11Buffer,
    sampler: ID3D11SamplerState,
    blend: ID3D11BlendState,
    raster: ID3D11RasterizerState,
    /// 没有海报层的帧（海报墙）也要绑定一个有效 SRV。
    dummy: ID3D11ShaderResourceView,
    width: u32,
    height: u32,
}

impl Gpu {
    pub fn new(hwnd: HWND, width: u32, height: u32) -> Result<Self, GpuError> {
        let shaders = shader_bytes()?;
        unsafe {
            let desc = DXGI_SWAP_CHAIN_DESC {
                BufferDesc: DXGI_MODE_DESC {
                    Width: width,
                    Height: height,
                    RefreshRate: DXGI_RATIONAL {
                        Numerator: 0,
                        Denominator: 0,
                    },
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    ScanlineOrdering: DXGI_MODE_SCANLINE_ORDER_UNSPECIFIED,
                    Scaling: DXGI_MODE_SCALING_UNSPECIFIED,
                },
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                OutputWindow: hwnd,
                Windowed: TRUE,
                SwapEffect: DXGI_SWAP_EFFECT_DISCARD,
                Flags: 0,
            };

            let mut swap = None;
            let mut device = None;
            let mut context = None;
            let levels = [D3D_FEATURE_LEVEL_11_0];
            D3D11CreateDeviceAndSwapChain(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                None,
                Default::default(),
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&desc),
                Some(&mut swap),
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .map_err(|e| GpuError::from_win("创建 D3D11 设备", e))?;

            let swap: IDXGISwapChain =
                swap.ok_or_else(|| GpuError::text("创建 D3D11 设备", "没有拿到交换链"))?;
            let device: ID3D11Device =
                device.ok_or_else(|| GpuError::text("创建 D3D11 设备", "没有拿到 D3D 设备"))?;
            let context: ID3D11DeviceContext =
                context.ok_or_else(|| GpuError::text("创建 D3D11 设备", "没有拿到 D3D 上下文"))?;

            let vs = {
                let mut shader = None;
                device
                    .CreateVertexShader(&shaders.vs, None, Some(&mut shader))
                    .map_err(|e| GpuError::from_win("创建顶点着色器", e))?;
                shader.ok_or_else(|| GpuError::text("创建顶点着色器", "顶点着色器为空"))?
            };
            let ps = {
                let mut shader = None;
                device
                    .CreatePixelShader(&shaders.ps, None, Some(&mut shader))
                    .map_err(|e| GpuError::from_win("创建像素着色器", e))?;
                shader.ok_or_else(|| GpuError::text("创建像素着色器", "像素着色器为空"))?
            };

            let cb = {
                let desc = D3D11_BUFFER_DESC {
                    ByteWidth: std::mem::size_of::<Params>() as u32,
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                    CPUAccessFlags: 0,
                    MiscFlags: 0,
                    StructureByteStride: 0,
                };
                let mut buffer = None;
                device
                    .CreateBuffer(&desc, None, Some(&mut buffer as *mut _))
                    .map_err(|e| GpuError::from_win("创建常量缓冲", e))?;
                buffer.ok_or_else(|| GpuError::text("创建常量缓冲", "常量缓冲为空"))?
            };

            let sampler = {
                let desc = D3D11_SAMPLER_DESC {
                    Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
                    AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                    AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                    AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                    MipLODBias: 0.0,
                    MaxAnisotropy: 1,
                    ComparisonFunc: D3D11_COMPARISON_ALWAYS,
                    BorderColor: [0.0; 4],
                    MinLOD: 0.0,
                    MaxLOD: f32::MAX,
                };
                let mut state = None;
                device
                    .CreateSamplerState(&desc, Some(&mut state as *mut _))
                    .map_err(|e| GpuError::from_win("创建采样器", e))?;
                state.ok_or_else(|| GpuError::text("创建采样器", "采样器为空"))?
            };

            let blend = {
                let rt = D3D11_RENDER_TARGET_BLEND_DESC {
                    BlendEnable: TRUE,
                    SrcBlend: D3D11_BLEND_SRC_ALPHA,
                    DestBlend: D3D11_BLEND_INV_SRC_ALPHA,
                    BlendOp: D3D11_BLEND_OP_ADD,
                    SrcBlendAlpha: D3D11_BLEND_SRC_ALPHA,
                    DestBlendAlpha: D3D11_BLEND_INV_SRC_ALPHA,
                    BlendOpAlpha: D3D11_BLEND_OP_ADD,
                    RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
                };
                let desc = D3D11_BLEND_DESC {
                    AlphaToCoverageEnable: FALSE,
                    IndependentBlendEnable: FALSE,
                    RenderTarget: [rt; 8],
                };
                let mut state = None;
                device
                    .CreateBlendState(&desc, Some(&mut state as *mut _))
                    .map_err(|e| GpuError::from_win("创建混合状态", e))?;
                state.ok_or_else(|| GpuError::text("创建混合状态", "混合状态为空"))?
            };

            let raster = {
                let desc = D3D11_RASTERIZER_DESC {
                    FillMode: D3D11_FILL_SOLID,
                    CullMode: D3D11_CULL_NONE,
                    FrontCounterClockwise: FALSE,
                    DepthBias: 0,
                    DepthBiasClamp: 0.0,
                    SlopeScaledDepthBias: 0.0,
                    DepthClipEnable: TRUE,
                    ScissorEnable: FALSE,
                    MultisampleEnable: FALSE,
                    AntialiasedLineEnable: FALSE,
                };
                let mut state = None;
                device
                    .CreateRasterizerState(&desc, Some(&mut state as *mut _))
                    .map_err(|e| GpuError::from_win("创建光栅化状态", e))?;
                state.ok_or_else(|| GpuError::text("创建光栅化状态", "光栅化状态为空"))?
            };

            let dummy = {
                let pixel = [0u32; 1];
                create_texture(&device, 1, 1, pixel.as_ptr() as *const u8, 4)?
            };

            Ok(Self {
                swap,
                device,
                context,
                vs,
                ps,
                cb,
                sampler,
                blend,
                raster,
                dummy,
                width,
                height,
            })
        }
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// 跟随窗口/显示器尺寸重建交换链缓冲区。
    ///
    /// 调用前先清空管线状态，确保没有残留的 render target 引用；
    /// `ResizeBuffers` 成功后，下一帧 `render_target()` 会拿到新尺寸的后缓冲。
    pub fn resize(&mut self, width: u32, height: u32) -> Result<(), GpuError> {
        if width == 0 || height == 0 {
            return Err(GpuError::text(
                "调整交换链尺寸",
                format!("无效的交换链尺寸: {width}x{height}"),
            ));
        }
        if width == self.width && height == self.height {
            return Ok(());
        }
        unsafe {
            self.context.ClearState();
            self.context.Flush();
            let result = self.swap.ResizeBuffers(
                0,
                width,
                height,
                DXGI_FORMAT_UNKNOWN,
                DXGI_SWAP_CHAIN_FLAG(0),
            );
            if let Err(err) = result {
                return Err(self.error_from("调整交换链尺寸", err));
            }
        }
        self.width = width;
        self.height = height;
        Ok(())
    }

    /// 把预取线程合成好的一帧上传成纹理，一张图只做一次。
    pub fn upload(&self, frame: &Frame) -> Result<Textures, GpuError> {
        let expected = self.width as usize * self.height as usize;
        match frame {
            Frame::Single(comp) => {
                if comp.background.len() != expected {
                    return Err(GpuError::text(
                        "上传纹理",
                        format!(
                            "背景缓冲区尺寸不匹配: 纹理 {}x{} 需要 {} 像素，实际 {} 像素",
                            self.width,
                            self.height,
                            expected,
                            comp.background.len()
                        ),
                    ));
                }

                let bg_srv = create_texture(
                    &self.device,
                    self.width,
                    self.height,
                    comp.background.as_ptr() as *const u8,
                    self.width * 4,
                )?;

                let (pw, ph) = (comp.poster.width(), comp.poster.height());
                let mut pixels = Vec::with_capacity((pw * ph) as usize);
                for px in comp.poster.pixels() {
                    let [r, g, b] = px.0;
                    pixels.push((b as u32) | ((g as u32) << 8) | ((r as u32) << 16));
                }
                let poster_srv =
                    create_texture(&self.device, pw, ph, pixels.as_ptr() as *const u8, pw * 4)?;

                Ok(Textures {
                    bg: bg_srv,
                    poster: Some(poster_srv),
                    rect: [
                        comp.position.0 as f32,
                        comp.position.1 as f32,
                        pw as f32,
                        ph as f32,
                    ],
                    wall: false,
                })
            }
            Frame::Wall(wall) => {
                if (wall.width, wall.height) != (self.width, self.height) {
                    return Err(GpuError::text(
                        "上传纹理",
                        format!(
                            "海报墙画布尺寸不匹配: 纹理 {}x{}，画布 {}x{}",
                            self.width, self.height, wall.width, wall.height
                        ),
                    ));
                }
                if wall.canvas.len() != expected {
                    return Err(GpuError::text(
                        "上传纹理",
                        format!(
                            "海报墙缓冲区尺寸不匹配: 需要 {} 像素，实际 {} 像素",
                            expected,
                            wall.canvas.len()
                        ),
                    ));
                }
                let bg_srv = create_texture(
                    &self.device,
                    wall.width,
                    wall.height,
                    wall.canvas.as_ptr() as *const u8,
                    wall.width * 4,
                )?;
                Ok(Textures {
                    bg: bg_srv,
                    poster: None,
                    rect: [0.0, 0.0, wall.width as f32, wall.height as f32],
                    wall: true,
                })
            }
        }
    }

    /// 画一层画面。转场时先画旧层，再画新层。
    ///
    /// `bg_alpha` 控制背景（海报墙时是整张画布）；`poster_alpha` 只影响单张布局的
    /// 海报图层。`offset` 是相对最终居中位置的屏幕像素偏移。
    pub fn draw(
        &self,
        tex: &Textures,
        uv: [f32; 4],
        bg_alpha: f32,
        poster_alpha: f32,
        offset: (f32, f32),
    ) {
        unsafe {
            let Some(rtv) = self.render_target() else {
                return;
            };

            let viewport = D3D11_VIEWPORT_ {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: self.width as f32,
                Height: self.height as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            };

            let params = Params {
                screen: [self.width as f32, self.height as f32],
                // 海报墙移动的是整张画布，背景 UV 也要跟着偏移；
                // 单张布局背景始终铺满，只有海报图层移动。
                bg_offset: if tex.wall {
                    [offset.0, offset.1]
                } else {
                    [0.0, 0.0]
                },
                rect: if tex.wall {
                    [0.0; 4]
                } else {
                    [
                        tex.rect[0] + offset.0,
                        tex.rect[1] + offset.1,
                        tex.rect[2],
                        tex.rect[3],
                    ]
                },
                uv,
                color_alpha: [
                    poster_alpha.clamp(0.0, 1.0),
                    1.0,
                    1.0,
                    bg_alpha.clamp(0.0, 1.0),
                ],
            };

            self.context.UpdateSubresource(
                &self.cb,
                0,
                None,
                &params as *const _ as *const _,
                0,
                0,
            );
            self.context.OMSetRenderTargets(Some(&[Some(rtv)]), None);
            self.context.RSSetViewports(Some(&[viewport]));
            self.context
                .IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            self.context.VSSetShader(&self.vs, None);
            self.context.PSSetShader(&self.ps, None);
            let poster = tex.poster.clone().unwrap_or_else(|| self.dummy.clone());
            self.context
                .PSSetShaderResources(0, Some(&[Some(tex.bg.clone()), Some(poster)]));
            self.context
                .PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            self.context.OMSetBlendState(&self.blend, None, 0xffffffff);
            self.context.RSSetState(&self.raster);
            self.context
                .PSSetConstantBuffers(0, Some(&[Some(self.cb.clone())]));
            self.context.Draw(3, 0);
        }
    }

    pub fn present(&self) -> Result<PresentStatus, GpuError> {
        let result = unsafe { self.swap.Present(1, Default::default()) };
        if result == DXGI_STATUS_OCCLUDED {
            // 被其他全屏窗口遮住是成功状态，不是设备故障。
            return Ok(PresentStatus::Occluded);
        }
        if result.is_ok() {
            return Ok(PresentStatus::Presented);
        }
        Err(self.error_from("Present", windows::core::Error::from_hresult(result)))
    }

    /// 把尽量多的上下文带进错误：Present 失败后设备原因往往才是根因。
    fn error_from(&self, context: &'static str, err: windows::core::Error) -> GpuError {
        let mut error = GpuError::from_win(context, err);
        if let Some(reason) = self.device_removed_reason() {
            error.device_lost = true;
            error.message = format!("{}；GetDeviceRemovedReason=0x{reason:08X}", error.message);
        }
        error
    }

    fn device_removed_reason(&self) -> Option<i32> {
        unsafe { self.device.GetDeviceRemovedReason() }
            .err()
            .map(|err| err.code().0)
    }

    fn render_target(&self) -> Option<ID3D11RenderTargetView> {
        unsafe {
            let texture: ID3D11Texture2D = self.swap.GetBuffer(0).ok()?;
            let mut rtv = None;
            self.device
                .CreateRenderTargetView(&texture, None, Some(&mut rtv))
                .ok()?;
            rtv
        }
    }
}

fn compile(source: &str, entry: &str, target: &str) -> Result<ID3DBlob, GpuError> {
    unsafe {
        let mut code = None;
        let mut errors = None;
        let entry_c = std::ffi::CString::new(entry).unwrap();
        let target_c = std::ffi::CString::new(target).unwrap();
        let name_c = std::ffi::CString::new("shader").unwrap();

        let result = D3DCompile(
            source.as_ptr() as *const _,
            source.len(),
            PCSTR(name_c.as_ptr() as *const u8),
            None,
            None,
            PCSTR(entry_c.as_ptr() as *const u8),
            PCSTR(target_c.as_ptr() as *const u8),
            0,
            0,
            &mut code,
            Some(&mut errors),
        );

        if let Err(err) = result {
            let message = errors
                .map(|b| blob_string(&b))
                .unwrap_or_else(|| err.message());
            return Err(GpuError {
                context: "编译着色器",
                hresult: err.code().0,
                device_lost: false,
                message,
            });
        }
        code.ok_or_else(|| GpuError::text("编译着色器", "着色器编译结果为空"))
    }
}

fn blob_bytes(blob: &ID3DBlob) -> &[u8] {
    unsafe {
        let ptr = blob.GetBufferPointer() as *const u8;
        let len = blob.GetBufferSize();
        std::slice::from_raw_parts(ptr, len)
    }
}

fn blob_string(blob: &ID3DBlob) -> String {
    let bytes = blob_bytes(blob);
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).to_string()
}

fn create_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    data: *const u8,
    row_pitch: u32,
) -> Result<ID3D11ShaderResourceView, GpuError> {
    unsafe {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_IMMUTABLE,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let init = D3D11_SUBRESOURCE_DATA {
            pSysMem: data as *const _,
            SysMemPitch: row_pitch,
            SysMemSlicePitch: 0,
        };
        let mut texture = None;
        device
            .CreateTexture2D(&desc, Some(&init), Some(&mut texture))
            .map_err(|e| GpuError::from_win("创建纹理", e))?;
        let texture: ID3D11Texture2D =
            texture.ok_or_else(|| GpuError::text("创建纹理", "纹理为空"))?;
        let mut view = None;
        device
            .CreateShaderResourceView(&texture, None, Some(&mut view))
            .map_err(|e| GpuError::from_win("创建纹理视图", e))?;
        view.ok_or_else(|| GpuError::text("创建纹理视图", "纹理视图为空"))
    }
}

#[allow(dead_code)]
const _: u32 = D3D11_BIND_RENDER_TARGET.0 as u32;
#[allow(dead_code)]
const _: u32 = DXGI_FORMAT_UNKNOWN.0 as u32;
#[allow(dead_code)]
const _: D3D_FEATURE_LEVEL = D3D_FEATURE_LEVEL_11_0;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_loss_codes_are_classified() {
        assert!(is_device_loss(DXGI_ERROR_DEVICE_REMOVED.0));
        assert!(is_device_loss(DXGI_ERROR_DEVICE_HUNG.0));
        assert!(is_device_loss(DXGI_ERROR_DEVICE_RESET.0));
        assert!(is_device_loss(DXGI_ERROR_DRIVER_INTERNAL_ERROR.0));
        assert!(!is_device_loss(0x8007_0057u32 as i32)); // E_INVALIDARG
        assert!(!is_device_loss(DXGI_STATUS_OCCLUDED.0));
    }

    #[test]
    fn shader_cache_reuses_success_and_retries_failures() {
        let slot = Mutex::new(None);
        let mut calls = 0;
        let first = cache_or_build(&slot, || {
            calls += 1;
            Ok::<u32, String>(7)
        })
        .unwrap();
        let second = cache_or_build(&slot, || {
            calls += 1;
            Ok::<u32, String>(9)
        })
        .unwrap();
        assert_eq!((first, second), (7, 7));
        assert_eq!(calls, 1, "成功后应复用缓存的字节码");

        let slot: Mutex<Option<u32>> = Mutex::new(None);
        let mut calls = 0;
        let failed = cache_or_build(&slot, || {
            calls += 1;
            Err::<u32, String>("boom".to_string())
        });
        assert!(failed.is_err());
        let recovered = cache_or_build(&slot, || {
            calls += 1;
            Ok::<u32, String>(5)
        })
        .unwrap();
        assert_eq!(recovered, 5);
        assert_eq!(calls, 2, "编译失败不能被永久缓存");
    }
}
