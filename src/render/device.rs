//! Direct3D, DXGI, Direct2D and DirectComposition set-up.
//!
//! A [`Device`] is a Direct3D 11 device with a Direct2D device context over
//! it, on the hardware or on WARP, the software rasteriser, which is what
//! machines without a GPU (such as CI runners) have.
//!
//! Creating a hardware device loads the graphics driver, which can take well
//! over 100 ms, while a WARP device takes a few. So the viewer draws its first
//! frame on WARP while [`hardware_device`] runs on another thread, then moves
//! to the hardware. A [`SwapChain`] is a flip-model swap chain made for
//! composition, and a [`Composition`] shows one in a window; switching the
//! window from one swap chain to another is a single atomic commit, so the
//! move between devices never shows an empty frame.

use windows::Win32::Foundation::{HMODULE, HWND};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D_SIZE_U, D2D1_ALPHA_MODE_IGNORE, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_PIXEL_FORMAT,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1_BITMAP_OPTIONS, D2D1_BITMAP_OPTIONS_CANNOT_DRAW, D2D1_BITMAP_OPTIONS_NONE,
    D2D1_BITMAP_OPTIONS_TARGET, D2D1_BITMAP_PROPERTIES1, D2D1_DEVICE_CONTEXT_OPTIONS_NONE,
    D2D1_FACTORY_OPTIONS, D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1CreateFactory, ID2D1Bitmap1,
    ID2D1DeviceContext, ID2D1Factory1,
};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device,
};
use windows::Win32::Graphics::DirectComposition::{
    DCompositionCreateDevice2, IDCompositionDesktopDevice, IDCompositionTarget,
    IDCompositionVisual2,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_PRESENT, DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG,
    DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT, IDXGIDevice, IDXGIFactory2,
    IDXGISurface, IDXGISwapChain1,
};
use windows::core::{IUnknown, Interface};

use crate::decode::wic::Decoded;
use crate::error::{Error, Result};
use crate::model::geometry::PixelSize;

/// The pixel format of every bitmap Seer draws or draws to: BGRA, 8 bits a
/// channel, premultiplied alpha.
pub const PIXEL_FORMAT: D2D1_PIXEL_FORMAT = D2D1_PIXEL_FORMAT {
    format: DXGI_FORMAT_B8G8R8A8_UNORM,
    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
};

/// Creates a Direct3D device on the hardware. This is the slow part of making
/// a hardware [`Device`], and the only part that may run on another thread:
/// Direct3D 11 devices are free-threaded, while the Direct2D objects over one
/// belong to the thread that draws.
pub fn hardware_device() -> Result<ID3D11Device> {
    d3d_device(D3D_DRIVER_TYPE_HARDWARE)
}

/// A Direct3D device and a Direct2D device context that draws through it.
pub struct Device {
    d3d: ID3D11Device,
    context: ID2D1DeviceContext,
    warp: bool,
}

impl Device {
    /// Creates a device on the hardware, or on WARP if the hardware can't
    /// provide one.
    pub fn new() -> Result<Self> {
        hardware_device()
            .and_then(|d3d| Device::over(d3d, false))
            .or_else(|_| Device::warp())
    }

    /// Creates a device on WARP.
    pub fn warp() -> Result<Self> {
        Device::over(d3d_device(D3D_DRIVER_TYPE_WARP)?, true)
    }

    /// Creates a device over a hardware Direct3D device from
    /// [`hardware_device`].
    pub fn from_hardware(d3d: ID3D11Device) -> Result<Self> {
        Device::over(d3d, false)
    }

    fn over(d3d: ID3D11Device, warp: bool) -> Result<Self> {
        let dxgi: IDXGIDevice = d3d.cast()?;
        // SAFETY: the options are a live local.
        let factory: ID2D1Factory1 = unsafe {
            D2D1CreateFactory(
                D2D1_FACTORY_TYPE_SINGLE_THREADED,
                Some(&D2D1_FACTORY_OPTIONS::default()),
            )
        }?;
        // SAFETY: the DXGI device is live.
        let device = unsafe { factory.CreateDevice(&dxgi) }?;
        // SAFETY: CreateDeviceContext takes plain options.
        let context = unsafe { device.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE) }?;
        Ok(Device { d3d, context, warp })
    }

    /// The Direct2D device context.
    pub fn context(&self) -> &ID2D1DeviceContext {
        &self.context
    }

    /// Whether the device is WARP rather than hardware.
    pub fn is_warp(&self) -> bool {
        self.warp
    }

    /// Whether the device has been lost, as when the graphics driver is
    /// updated or restarts, or the adapter is removed. A lost device draws
    /// nothing again; everything made on it must be made again on a new one.
    pub fn is_lost(&self) -> bool {
        // SAFETY: GetDeviceRemovedReason has no arguments.
        unsafe { self.d3d.GetDeviceRemovedReason() }.is_err()
    }

    /// A bitmap holding a decoded image.
    pub fn create_bitmap(&self, decoded: &Decoded) -> Result<ID2D1Bitmap1> {
        let size = decoded.bitmap_size;
        let properties = bitmap_properties(D2D1_BITMAP_OPTIONS_NONE);
        // SAFETY: the pixels hold `size.height` rows of `size.width × 4`
        // bytes, which Direct2D copies before returning.
        let bitmap = unsafe {
            self.context.CreateBitmap(
                d2d_size(size),
                Some(decoded.pixels.as_ptr().cast()),
                size.width * 4,
                &properties,
            )
        }?;
        Ok(bitmap)
    }

    /// An empty bitmap the context can draw to.
    pub fn create_target(&self, size: PixelSize) -> Result<ID2D1Bitmap1> {
        let properties = bitmap_properties(D2D1_BITMAP_OPTIONS_TARGET);
        // SAFETY: no source data is given, and the properties are a live
        // local.
        Ok(unsafe {
            self.context
                .CreateBitmap(d2d_size(size), None, 0, &properties)
        }?)
    }
}

fn d3d_device(driver: D3D_DRIVER_TYPE) -> Result<ID3D11Device> {
    let mut d3d = None;
    // SAFETY: the out pointer is to a live local; no adapter, software module
    // or feature levels are given, so the defaults apply.
    unsafe {
        D3D11CreateDevice(
            None,
            driver,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut d3d),
            None,
            None,
        )
    }?;
    d3d.ok_or_else(|| Error::unexpected("Direct3D returned no device"))
}

/// A flip-model swap chain for composition, drawn through a device's context.
pub struct SwapChain {
    chain: IDXGISwapChain1,
    size: PixelSize,
}

impl SwapChain {
    /// Creates a swap chain of `size` on `device`.
    pub fn new(device: &Device, size: PixelSize) -> Result<Self> {
        let dxgi: IDXGIDevice = device.d3d.cast()?;
        // SAFETY: GetAdapter has no arguments.
        let adapter = unsafe { dxgi.GetAdapter() }?;
        // SAFETY: GetParent has no arguments.
        let factory: IDXGIFactory2 = unsafe { adapter.GetParent() }?;
        let size = at_least_one(size);
        let description = DXGI_SWAP_CHAIN_DESC1 {
            Width: size.width,
            Height: size.height,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            AlphaMode: DXGI_ALPHA_MODE_IGNORE,
            ..Default::default()
        };
        // SAFETY: the device and the description are live.
        let chain =
            unsafe { factory.CreateSwapChainForComposition(&device.d3d, &description, None) }?;
        Ok(SwapChain { chain, size })
    }

    /// The back buffers' size.
    pub fn size(&self) -> PixelSize {
        self.size
    }

    /// Resizes the back buffers, if `size` differs from theirs. The device
    /// context's target must not be a back buffer.
    pub fn resize(&mut self, size: PixelSize) -> Result<()> {
        let size = at_least_one(size);
        if size == self.size {
            return Ok(());
        }
        // SAFETY: plain values; zero buffers and an unknown format keep the
        // existing ones.
        unsafe {
            self.chain.ResizeBuffers(
                0,
                size.width,
                size.height,
                DXGI_FORMAT_UNKNOWN,
                DXGI_SWAP_CHAIN_FLAG(0),
            )
        }?;
        self.size = size;
        Ok(())
    }

    /// Sets the back buffer as `device`'s target. Release it with
    /// [`present`](SwapChain::present) before resizing.
    pub fn target(&self, device: &Device) -> Result<()> {
        // SAFETY: buffer 0 is the back buffer of a flip-model chain.
        let surface: IDXGISurface = unsafe { self.chain.GetBuffer(0) }?;
        let properties = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_IGNORE,
            },
            dpiX: 96.0,
            dpiY: 96.0,
            bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
            ..Default::default()
        };
        // SAFETY: the surface and the properties are live.
        let bitmap = unsafe {
            device
                .context
                .CreateBitmapFromDxgiSurface(&surface, Some(&properties))
        }?;
        // SAFETY: the bitmap is a target on this context's device.
        unsafe { device.context.SetTarget(&bitmap) };
        Ok(())
    }

    /// Releases `device`'s target and presents the back buffer, without
    /// waiting for a vertical blank.
    pub fn present(&self, device: &Device) -> Result<()> {
        // SAFETY: releasing the target only drops the context's reference to
        // the back buffer, which resizing requires.
        unsafe { device.context.SetTarget(None) };
        // SAFETY: plain values.
        unsafe { self.chain.Present(0, DXGI_PRESENT(0)) }.ok()?;
        Ok(())
    }
}

/// What a window shows: one visual whose content is a swap chain.
pub struct Composition {
    device: IDCompositionDesktopDevice,
    visual: IDCompositionVisual2,
    _target: IDCompositionTarget,
}

impl Composition {
    /// Composes `window`'s content, which shows nothing until
    /// [`show`](Composition::show) is called.
    pub fn new(window: HWND) -> Result<Self> {
        // SAFETY: no rendering device is given; content comes from swap
        // chains, which carry their own.
        let device: IDCompositionDesktopDevice =
            unsafe { DCompositionCreateDevice2(None::<&IUnknown>) }?;
        // SAFETY: the window belongs to this process. Not topmost: the
        // content goes behind the window's child windows, such as the note
        // field, which would otherwise be hidden under it.
        let target = unsafe { device.CreateTargetForHwnd(window, false) }?;
        // SAFETY: CreateVisual has no arguments.
        let visual = unsafe { device.CreateVisual() }?;
        // SAFETY: the visual was made by the same device as the target.
        unsafe { target.SetRoot(&visual) }?;
        Ok(Composition {
            device,
            visual,
            _target: target,
        })
    }

    /// Shows `chain`'s last presented frame in place of what was shown.
    pub fn show(&self, chain: &SwapChain) -> Result<()> {
        // SAFETY: the swap chain was made for composition; the visual keeps a
        // reference to it.
        unsafe { self.visual.SetContent(&chain.chain) }?;
        // SAFETY: Commit has no arguments.
        unsafe { self.device.Commit() }?;
        Ok(())
    }
}

/// Bitmap properties at 96 DPI, so one device-independent pixel is one pixel.
pub(crate) fn bitmap_properties(options: D2D1_BITMAP_OPTIONS) -> D2D1_BITMAP_PROPERTIES1 {
    D2D1_BITMAP_PROPERTIES1 {
        pixelFormat: PIXEL_FORMAT,
        dpiX: 96.0,
        dpiY: 96.0,
        bitmapOptions: options,
        ..Default::default()
    }
}

pub(crate) fn d2d_size(size: PixelSize) -> D2D_SIZE_U {
    D2D_SIZE_U {
        width: size.width,
        height: size.height,
    }
}

/// Direct3D refuses empty surfaces, which a minimised window would ask for.
fn at_least_one(size: PixelSize) -> PixelSize {
    PixelSize::new(size.width.max(1), size.height.max(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn devices_are_created_with_a_warp_fallback() {
        let device = Device::new().unwrap();
        let _ = device.is_warp();
        assert!(!device.is_lost());
        assert!(Device::warp().unwrap().is_warp());
        if let Ok(d3d) = hardware_device() {
            assert!(!Device::from_hardware(d3d).unwrap().is_warp());
        }
    }

    #[test]
    fn decoded_images_become_bitmaps() {
        let device = Device::warp().unwrap();
        let decoded = Decoded {
            size: PixelSize::new(4, 2),
            bitmap_size: PixelSize::new(2, 1),
            pixels: vec![255; 8],
        };
        let bitmap = device.create_bitmap(&decoded).unwrap();
        // SAFETY: GetPixelSize has no arguments.
        let size = unsafe { bitmap.GetPixelSize() };
        assert_eq!((size.width, size.height), (2, 1));
    }

    #[test]
    fn swap_chains_are_composed_presented_and_resized() {
        use crate::model::geometry::PixelRect;
        use crate::viewer::window::Window;
        let window = Window::create(PixelRect::new(0, 0, 320, 200), "swap chain").unwrap();
        let composition = Composition::new(window.hwnd()).unwrap();
        for device in [Device::warp().unwrap(), Device::new().unwrap()] {
            let mut chain = SwapChain::new(&device, window.client_size()).unwrap();
            assert_eq!(chain.size(), PixelSize::new(320, 200));
            let context = device.context();
            chain.target(&device).unwrap();
            // SAFETY: drawing nothing between BeginDraw and EndDraw.
            unsafe {
                context.BeginDraw();
                context.EndDraw(None, None).unwrap();
            }
            chain.present(&device).unwrap();
            composition.show(&chain).unwrap();
            chain.resize(PixelSize::new(32, 0)).unwrap();
            assert_eq!(chain.size(), PixelSize::new(32, 1));
            chain.resize(PixelSize::new(32, 1)).unwrap();
        }
    }

    #[test]
    fn empty_sizes_become_one_pixel() {
        assert_eq!(at_least_one(PixelSize::new(0, 5)), PixelSize::new(1, 5));
    }
}
