//! Screen capture on Windows with DXGI Desktop Duplication, behind the same
//! slot API as the Wayland capturer, so the stream loop is shared.
//!
//! A frame is copied out only when Windows reports the desktop changed, so a
//! static screen costs nothing. While the desktop can't be duplicated (a UAC
//! prompt, the lock screen, a mode change), requests wait instead of failing,
//! and the stream picks up again when the desktop comes back.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use ffmpeg_next::format::Pixel;
use windows::core::Interface;
use windows::Win32::Foundation::{HMODULE, POINT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource, DXGI_ERROR_ACCESS_LOST,
    DXGI_ERROR_NOT_FOUND, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
};
use windows::Win32::Graphics::Gdi::{EnumDisplaySettingsW, DEVMODEW, ENUM_CURRENT_SETTINGS};
use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

use ac_remote::proto::MonitorInfo;

use crate::monitors::Monitor;

pub const SLOTS: usize = 2;

/// Pixel layout of a captured buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BufferInfo {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
}

impl BufferInfo {
    /// Desktop Duplication always hands out B8G8R8A8.
    pub fn pixel(&self) -> Pixel {
        Pixel::BGRA
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotState {
    Idle,
    /// Waiting for the desktop to change.
    Copying,
    Ready,
    Failed,
}

struct Slot {
    state: SlotState,
    info: BufferInfo,
    data: Vec<u8>,
    ready_us: u64,
}

impl Default for Slot {
    fn default() -> Self {
        Self { state: SlotState::Idle, info: BufferInfo { width: 0, height: 0, stride: 0 }, data: Vec::new(), ready_us: 0 }
    }
}

/// One desktop output of one graphics adapter.
struct Output {
    name: String,
    adapter: IDXGIAdapter1,
    output: IDXGIOutput,
}

/// Duplication of the output being streamed.
struct Dup {
    name: String,
    ctx: ID3D11DeviceContext,
    dup: IDXGIOutputDuplication,
    device: ID3D11Device,
    staging: Option<(ID3D11Texture2D, u32, u32)>,
}

pub struct Capturer {
    outputs: Vec<Output>,
    dup: Option<Dup>,
    slots: [Slot; SLOTS],
    pending: Option<usize>,
    /// The display last requested.
    want: String,
    /// Copy every frame instead of waiting for changes (benchmarking).
    pub continuous: bool,
}

/// A captured frame, borrowed from its slot.
pub struct Captured<'a> {
    pub info: BufferInfo,
    pub data: &'a [u8],
    pub y_invert: bool,
    pub ready_us: u64,
}

/// "\\.\DISPLAY1" → "DISPLAY1".
fn output_name(device: &[u16]) -> String {
    let end = device.iter().position(|&c| c == 0).unwrap_or(device.len());
    let s = String::from_utf16_lossy(&device[..end]);
    s.trim_start_matches(r"\\.\").to_string()
}

fn enum_outputs() -> Result<Vec<Output>> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.context("creating a DXGI factory")?;
    let mut out = Vec::new();
    for a in 0.. {
        let adapter = match unsafe { factory.EnumAdapters1(a) } {
            Ok(x) => x,
            Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(e) => return Err(e.into()),
        };
        for o in 0.. {
            let output = match unsafe { adapter.EnumOutputs(o) } {
                Ok(x) => x,
                Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(e) => return Err(e.into()),
            };
            let desc = unsafe { output.GetDesc() }?;
            if desc.AttachedToDesktop.as_bool() {
                out.push(Output { name: output_name(&desc.DeviceName), adapter: adapter.clone(), output });
            }
        }
    }
    Ok(out)
}

/// The desktop's monitors, in physical pixels (the service is DPI aware).
pub fn monitors() -> Result<Vec<Monitor>> {
    let mut cursor = POINT::default();
    let _ = unsafe { GetCursorPos(&mut cursor) };
    let mut mons = Vec::new();
    for o in enum_outputs()? {
        let desc = unsafe { o.output.GetDesc() }?;
        let r = desc.DesktopCoordinates;
        let (w, h) = ((r.right - r.left).max(0) as u32, (r.bottom - r.top).max(0) as u32);
        let mut mode = DEVMODEW { dmSize: std::mem::size_of::<DEVMODEW>() as u16, ..Default::default() };
        let hz = if unsafe { EnumDisplaySettingsW(windows::core::PCWSTR(desc.DeviceName.as_ptr()), ENUM_CURRENT_SETTINGS, &mut mode) }.as_bool() {
            mode.dmDisplayFrequency as f32
        } else {
            60.0
        };
        let adapter = unsafe { o.adapter.GetDesc1() }.map(|d| output_name(&d.Description)).unwrap_or_default();
        let focused = cursor.x >= r.left && cursor.x < r.right && cursor.y >= r.top && cursor.y < r.bottom;
        mons.push(Monitor {
            logical_w: w,
            logical_h: h,
            info: MonitorInfo { name: o.name, description: adapter, x: r.left, y: r.top, width: w, height: h, refresh_hz: hz, focused },
        });
    }
    Ok(mons)
}

impl Capturer {
    pub fn connect() -> Result<Self> {
        Ok(Self { outputs: enum_outputs()?, dup: None, slots: Default::default(), pending: None, want: String::new(), continuous: false })
    }

    pub fn output_names(&self) -> Vec<String> {
        self.outputs.iter().map(|o| o.name.clone()).collect()
    }

    pub fn slot_state(&self, slot: usize) -> SlotState {
        self.slots[slot].state
    }

    /// Ask for the next changed frame of `output` into `slot`. The cursor is
    /// never drawn in (Desktop Duplication delivers it separately).
    pub fn request(&mut self, slot: usize, output: &str, _overlay_cursor: bool) -> Result<()> {
        if self.dup.as_ref().is_some_and(|d| d.name != output) {
            self.dup = None;
        }
        if !self.outputs.iter().any(|o| o.name == output) {
            return Err(anyhow!("no display named {output}"));
        }
        self.slots[slot].state = SlotState::Copying;
        self.pending = Some(slot);
        self.want = output.to_string();
        self.try_duplicate();
        Ok(())
    }

    /// The secure desktop (UAC, lock screen) can't be duplicated: until it
    /// can, requests wait.
    fn try_duplicate(&mut self) {
        if self.dup.is_none() {
            self.dup = self.duplicate(&self.want).map_err(|e| tracing::debug!("duplicating {}: {e:#}", self.want)).ok();
        }
    }

    fn duplicate(&self, name: &str) -> Result<Dup> {
        let o = self.outputs.iter().find(|o| o.name == name).ok_or_else(|| anyhow!("no display named {name}"))?;
        let (mut device, mut ctx) = (None, None);
        unsafe {
            D3D11CreateDevice(
                &o.adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut ctx),
            )
        }
        .context("creating a Direct3D device")?;
        let device = device.context("no Direct3D device")?;
        let ctx = ctx.context("no Direct3D context")?;
        let output1: IDXGIOutput1 = o.output.cast()?;
        let dup = unsafe { output1.DuplicateOutput(&device) }.context("duplicating the desktop")?;
        Ok(Dup { name: name.to_string(), ctx, dup, device, staging: None })
    }

    /// Wait up to `timeout` for the pending request to complete.
    pub fn poll(&mut self, timeout: Duration) -> Result<()> {
        let Some(slot) = self.pending else {
            std::thread::sleep(timeout);
            return Ok(());
        };
        let Some(d) = self.dup.as_mut() else {
            std::thread::sleep(timeout.max(Duration::from_millis(50)));
            self.try_duplicate();
            return Ok(());
        };
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut res: Option<IDXGIResource> = None;
        match unsafe { d.dup.AcquireNextFrame(timeout.as_millis().min(1000) as u32, &mut info, &mut res) } {
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(()),
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                // Desktop switch or mode change: duplicate again, and keep
                // the request waiting meanwhile.
                self.dup = None;
                return Ok(());
            }
            Err(e) => return Err(anyhow!("capturing the desktop: {e}")),
        }
        // Only the pointer moved: nothing new to encode.
        let changed = info.LastPresentTime != 0 || self.continuous;
        let copied = match (changed, res) {
            (true, Some(res)) => copy_frame(d, &res, &mut self.slots[slot]),
            _ => Ok(false),
        };
        let _ = unsafe { d.dup.ReleaseFrame() };
        if copied? {
            self.slots[slot].state = SlotState::Ready;
            self.slots[slot].ready_us = ac_remote::clock::now_us();
            self.pending = None;
        }
        Ok(())
    }

    pub fn ready(&self, slot: usize) -> Option<Captured<'_>> {
        let s = &self.slots[slot];
        if s.state != SlotState::Ready {
            return None;
        }
        Some(Captured { info: s.info, data: &s.data, y_invert: false, ready_us: s.ready_us })
    }
}

/// Copy the acquired desktop image into `slot` through a CPU-readable texture.
fn copy_frame(d: &mut Dup, res: &IDXGIResource, slot: &mut Slot) -> Result<bool> {
    let tex: ID3D11Texture2D = res.cast()?;
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { tex.GetDesc(&mut desc) };
    if d.staging.as_ref().is_none_or(|(_, w, h)| (*w, *h) != (desc.Width, desc.Height)) {
        let sdesc = D3D11_TEXTURE2D_DESC {
            Usage: D3D11_USAGE_STAGING,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            BindFlags: 0,
            MiscFlags: 0,
            MipLevels: 1,
            ArraySize: 1,
            ..desc
        };
        let mut staging = None;
        unsafe { d.device.CreateTexture2D(&sdesc, None, Some(&mut staging)) }.context("creating a staging texture")?;
        d.staging = Some((staging.context("no staging texture")?, desc.Width, desc.Height));
    }
    let (staging, w, h) = d.staging.as_ref().unwrap();
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    unsafe {
        d.ctx.CopyResource(staging, &tex);
        d.ctx.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).context("reading the desktop image")?;
    }
    let stride = *w * 4;
    slot.info = BufferInfo { width: *w, height: *h, stride };
    slot.data.resize((stride * *h) as usize, 0);
    for row in 0..*h as usize {
        let src = unsafe { std::slice::from_raw_parts((mapped.pData as *const u8).add(row * mapped.RowPitch as usize), stride as usize) };
        slot.data[row * stride as usize..(row + 1) * stride as usize].copy_from_slice(src);
    }
    unsafe { d.ctx.Unmap(staging, 0) };
    Ok(true)
}
