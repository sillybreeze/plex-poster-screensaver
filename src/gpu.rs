//! D3D11 渲染：每张图上传一次纹理，每帧只更新几个 uniform，缩放交给 GPU 采样。

use image::RgbImage;
use windows::core::PCSTR;
use windows::Win32::Foundation::{HWND, FALSE, TRUE};
use windows::Win32::Graphics::Direct3D::{ID3DBlob, D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_11_0,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDeviceAndSwapChain, ID3D11BlendState, ID3D11Buffer, ID3D11Device,
    ID3D11DeviceContext, ID3D11PixelShader, ID3D11RenderTargetView, ID3D11SamplerState,
    ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11VertexShader, D3D11_BLEND_DESC,
    D3D11_BLEND_INV_SRC_ALPHA, D3D11_BLEND_OP_ADD, D3D11_BLEND_SRC_ALPHA, D3D11_BUFFER_DESC,
    D3D11_COLOR_WRITE_ENABLE_ALL, D3D11_COMPARISON_ALWAYS, D3D11_FILTER_MIN_MAG_MIP_LINEAR,
    D3D11_RENDER_TARGET_BLEND_DESC, D3D11_SAMPLER_DESC, D3D11_SUBRESOURCE_DATA,
    D3D11_TEXTURE2D_DESC, D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_USAGE_IMMUTABLE,
    D3D11_VIEWPORT as D3D11_VIEWPORT_, D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_RENDER_TARGET,
    D3D11_BIND_SHADER_RESOURCE, D3D11_SDK_VERSION, D3D11_CULL_NONE, D3D11_FILL_SOLID,
    D3D11_RASTERIZER_DESC, ID3D11RasterizerState,
    D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_UNKNOWN, DXGI_MODE_DESC, DXGI_MODE_SCALING_UNSPECIFIED,
    DXGI_MODE_SCANLINE_ORDER_UNSPECIFIED, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGISwapChain, DXGI_SWAP_CHAIN_DESC, DXGI_SWAP_CHAIN_FLAG, DXGI_SWAP_EFFECT_DISCARD,
    DXGI_USAGE_RENDER_TARGET_OUTPUT,
};

const SHADER: &str = r#"
cbuffer Params : register(b0)
{
    float2 screenSize;
    float2 pad0;
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
    float2 uv = i.pos.xy / screenSize;
    float3 c = bgTex.SampleLevel(samp, uv, 0).rgb;
    float2 q = (i.pos.xy - posterRect.xy) / posterRect.zw;
    if (q.x >= 0.0 && q.x <= 1.0 && q.y >= 0.0 && q.y <= 1.0)
    {
        float2 puv = posterUV.xy + q * posterUV.zw;
        c = posterTex.SampleLevel(samp, puv, 0).rgb;
    }
    return float4(c, colorAlpha.w);
}
"#;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Params {
    screen: [f32; 2],
    pad: [f32; 2],
    rect: [f32; 4],
    uv: [f32; 4],
    color_alpha: [f32; 4],
}

pub struct Textures {
    bg: ID3D11ShaderResourceView,
    poster: ID3D11ShaderResourceView,
    rect: [f32; 4],
}

impl Textures {
    /// 海报在屏幕上的矩形：[x, y, 宽, 高]
    pub fn rect(&self) -> [f32; 4] {
        self.rect
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
    width: u32,
    height: u32,
}

impl Gpu {
    pub fn new(hwnd: HWND, width: u32, height: u32) -> Result<Self, String> {
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
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
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
            .map_err(|e| format!("创建 D3D11 设备失败: {e}"))?;

            let swap: IDXGISwapChain = swap.ok_or("没有拿到交换链")?;
            let device: ID3D11Device = device.ok_or("没有拿到 D3D 设备")?;
            let context: ID3D11DeviceContext = context.ok_or("没有拿到 D3D 上下文")?;

            let vs_blob = compile(SHADER, "VS", "vs_4_0")?;
            let ps_blob = compile(SHADER, "PS", "ps_4_0")?;

            let vs = {
                let code = blob_bytes(&vs_blob);
                let mut shader = None;
                device
                    .CreateVertexShader(code, None, Some(&mut shader))
                    .map_err(|e| format!("创建顶点着色器失败: {e}"))?;
                shader.ok_or("顶点着色器为空")?
            };
            let ps = {
                let code = blob_bytes(&ps_blob);
                let mut shader = None;
                device
                    .CreatePixelShader(code, None, Some(&mut shader))
                    .map_err(|e| format!("创建像素着色器失败: {e}"))?;
                shader.ok_or("像素着色器为空")?
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
                    .map_err(|e| format!("创建常量缓冲失败: {e}"))?;
                buffer.ok_or("常量缓冲为空")?
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
                    .map_err(|e| format!("创建采样器失败: {e}"))?;
                state.ok_or("采样器为空")?
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
                    .map_err(|e| format!("创建混合状态失败: {e}"))?;
                state.ok_or("混合状态为空")?
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
                    .map_err(|e| format!("创建光栅化状态失败: {e}"))?;
                state.ok_or("光栅化状态为空")?
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
    pub fn resize(&mut self, width: u32, height: u32) -> Result<(), String> {
        if width == 0 || height == 0 {
            return Err(format!("无效的交换链尺寸: {width}x{height}"));
        }
        if width == self.width && height == self.height {
            return Ok(());
        }
        unsafe {
            self.context.ClearState();
            self.context.Flush();
            self.swap
                .ResizeBuffers(
                    0,
                    width,
                    height,
                    DXGI_FORMAT_UNKNOWN,
                    DXGI_SWAP_CHAIN_FLAG(0),
                )
                .map_err(|e| format!("调整交换链尺寸失败: {e}"))?;
        }
        self.width = width;
        self.height = height;
        Ok(())
    }

    /// 把背景（0x00RRGGBB）和海报（RGB）上传成纹理，一张图只做一次。
    pub fn upload(
        &self,
        bg: &[u32],
        poster: &RgbImage,
        position: (i32, i32),
    ) -> Result<Textures, String> {
        let expected = self.width as usize * self.height as usize;
        if bg.len() != expected {
            return Err(format!(
                "背景缓冲区尺寸不匹配: 纹理 {}x{} 需要 {} 像素，实际 {} 像素",
                self.width,
                self.height,
                expected,
                bg.len()
            ));
        }

        let bg_srv = create_texture(
            &self.device,
            self.width,
            self.height,
            bg.as_ptr() as *const u8,
            self.width * 4,
        )?;

        let (pw, ph) = (poster.width(), poster.height());
        let mut pixels = Vec::with_capacity((pw * ph) as usize);
        for px in poster.pixels() {
            let [r, g, b] = px.0;
            pixels.push((b as u32) | ((g as u32) << 8) | ((r as u32) << 16));
        }
        let poster_srv =
            create_texture(&self.device, pw, ph, pixels.as_ptr() as *const u8, pw * 4)?;

        Ok(Textures {
            bg: bg_srv,
            poster: poster_srv,
            rect: [
                position.0 as f32,
                position.1 as f32,
                pw as f32,
                ph as f32,
            ],
        })
    }

    /// 画一层画面。淡入淡出时调用两次（旧图 alpha=1，新图 alpha=t）。
    pub fn draw(&self, tex: &Textures, uv: [f32; 4], alpha: f32) {
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
                pad: [0.0, 0.0],
                rect: tex.rect,
                uv,
                color_alpha: [1.0, 1.0, 1.0, alpha.clamp(0.0, 1.0)],
            };

            self.context
                .UpdateSubresource(&self.cb, 0, None, &params as *const _ as *const _, 0, 0);
            self.context.OMSetRenderTargets(Some(&[Some(rtv)]), None);
            self.context.RSSetViewports(Some(&[viewport]));
            self.context
                .IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            self.context.VSSetShader(&self.vs, None);
            self.context.PSSetShader(&self.ps, None);
            self.context
                .PSSetShaderResources(0, Some(&[Some(tex.bg.clone()), Some(tex.poster.clone())]));
            self.context.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            self.context
                .OMSetBlendState(&self.blend, None, 0xffffffff);
            self.context.RSSetState(&self.raster);
            self.context
                .PSSetConstantBuffers(0, Some(&[Some(self.cb.clone())]));
            self.context.Draw(3, 0);
        }
    }

    pub fn present(&self) -> Result<(), String> {
        unsafe {
            let result = self.swap.Present(1, Default::default());
            if result.is_err() {
                Err(format!("Present 失败: 0x{:08X}", result.0))
            } else {
                Ok(())
            }
        }
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

fn compile(source: &str, entry: &str, target: &str) -> Result<ID3DBlob, String> {
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
                .unwrap_or_else(|| err.to_string());
            return Err(format!("着色器编译失败: {message}"));
        }
        code.ok_or_else(|| "着色器编译结果为空".to_string())
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
) -> Result<ID3D11ShaderResourceView, String> {
    unsafe {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
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
            .map_err(|e| format!("创建纹理失败: {e}"))?;
        let texture: ID3D11Texture2D = texture.ok_or("纹理为空")?;
        let mut view = None;
        device
            .CreateShaderResourceView(&texture, None, Some(&mut view))
            .map_err(|e| format!("创建纹理视图失败: {e}"))?;
        view.ok_or_else(|| "纹理视图为空".to_string())
    }
}

#[allow(dead_code)]
const _: u32 = D3D11_BIND_RENDER_TARGET.0 as u32;
#[allow(dead_code)]
const _: u32 = DXGI_FORMAT_UNKNOWN.0 as u32;
#[allow(dead_code)]
const _: D3D_FEATURE_LEVEL = D3D_FEATURE_LEVEL_11_0;
