//! 把离屏渲染出的面板快照写进剪贴板。
//!
//! 同时提供位图和「虚拟文件」：
//! - `CF_DIBV5`、`CF_DIB`、`CF_BITMAP`：聊天框、画图、Word 等按图粘贴
//! - 注册格式 `PNG`：现代应用优先取 PNG
//! - `FileGroupDescriptorW` + `FileContents`：资源管理器 Ctrl+V 落成 `CodexQuota.png`
//!
//! `OleSetClipboard` 只往剪贴板放数据对象指针（延迟渲染），源格式没有实体句柄，
//! 系统就不会再把 `CF_DIBV5` 合成 `CF_DIB`/`CF_BITMAP`。只认旧格式的接收方必须
//! 由我们在 `IDataObject` 里显式提供。资源管理器要的是文件，`FileContents` 必须
//! 是 `IStream`，所以走 OLE。PNG 用系统 WIC 编码，不引入额外图片库。
#![allow(clippy::inline_always, clippy::ref_as_ptr)]

use std::mem::{ManuallyDrop, size_of};
use std::ptr;
use std::sync::OnceLock;

use windows::Win32::Foundation::GlobalFree;
#[cfg(test)]
use windows::Win32::Foundation::HWND;
use windows::Win32::Foundation::{
    DATA_S_SAMEFORMATETC, DV_E_FORMATETC, E_INVALIDARG, E_NOTIMPL, E_OUTOFMEMORY, HGLOBAL,
    OLE_E_ADVISENOTSUPPORTED, S_OK,
};
use windows::Win32::Graphics::Gdi::{
    BI_BITFIELDS, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BITMAPV5HEADER, CreateDIBSection,
    DIB_RGB_COLORS, DeleteObject,
};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_ContainerFormatPng, GUID_WICPixelFormat32bppBGRA,
    IWICBitmapFrameEncode, IWICImagingFactory, WICBitmapEncoderNoCache,
};
use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL;
use windows::Win32::System::Com::StructuredStorage::{CreateStreamOnHGlobal, GetHGlobalFromStream};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, CoCreateInstance, DATADIR_GET, DVASPECT_CONTENT, FORMATETC, IAdviseSink,
    IDataObject, IDataObject_Impl, IEnumFORMATETC, IEnumSTATDATA, STATFLAG_NONAME, STATSTG,
    STGMEDIUM, STGMEDIUM_0, TYMED_GDI, TYMED_HGLOBAL, TYMED_ISTREAM,
};
use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
#[cfg(test)]
use windows::Win32::System::DataExchange::{CloseClipboard, OpenClipboard};
use windows::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows::Win32::System::Ole::{
    DROPEFFECT_COPY, OleInitialize, OleSetClipboard, OleUninitialize,
};
use windows::Win32::UI::Shell::{
    FD_ATTRIBUTES, FD_FILESIZE, FILEGROUPDESCRIPTORW, SHCreateMemStream, SHCreateStdEnumFmtEtc,
};
use windows::core::{BOOL, Error as WinError, HRESULT, PCWSTR, Ref, implement, w};

use super::renderer::PanelBitmap;
use crate::error::AppError;

/// `CF_DIBV5`（Windows 标准位图剪贴板格式）。
///
/// windows crate 只把这个常量挂在 `Win32::System::Ole` 下；为一个数字引入整个
/// Ole 模块（连带它的 feature）不划算，这里按 wingdi.h 的字面值写死。
const CF_BITMAP: u16 = 2;
const CF_DIB: u16 = 8;
const CF_DIBV5: u32 = 17;
const DIB_HEADER_LEN: usize = 40;

/// `LCS_sRGB`：声明像素本身就是 sRGB。
///
/// 留 0 表示 `LCS_CALIBRATED_RGB` 配全 0 的 endpoints/gamma，系统会认为需要按
/// 色彩空间转换。常量本身在 `Win32::UI::ColorSystem`，同样不为一个数字开 feature。
const LCS_SRGB: u32 = 0x7352_4742;

/// 资源管理器粘贴时使用的文件名。
const SNAPSHOT_FILE_NAME: &str = "CodexQuota.png";

/// UI 线程的 OLE 初始化守卫：`OleSetClipboard` 要求当前套间已经 `OleInitialize`。
///
/// 初始化失败不阻断启动，只让面板截图在复制时失败。
pub(super) struct OleGuard {
    initialized: bool,
}

impl OleGuard {
    pub(super) fn try_init() -> Self {
        // SAFETY: called once on the UI thread before creating windows or using OLE clipboard.
        match unsafe { OleInitialize(None) } {
            Ok(()) => Self { initialized: true },
            Err(error) => {
                crate::logging::log(&format!("无法初始化 OLE，面板截图将不可用：{error}"));
                Self { initialized: false }
            }
        }
    }
}

impl Drop for OleGuard {
    fn drop(&mut self) {
        if self.initialized {
            // SAFETY: balances a successful OleInitialize in OleGuard::try_init on this thread.
            unsafe { OleUninitialize() };
        }
    }
}

struct ClipboardFormats {
    png: u16,
    file_descriptor: u16,
    file_contents: u16,
    drop_effect: u16,
}

fn clipboard_formats() -> Result<&'static ClipboardFormats, AppError> {
    static FORMATS: OnceLock<Result<ClipboardFormats, u32>> = OnceLock::new();
    match FORMATS.get_or_init(|| {
        Ok(ClipboardFormats {
            png: register_format(w!("PNG"))?,
            file_descriptor: register_format(w!("FileGroupDescriptorW"))?,
            file_contents: register_format(w!("FileContents"))?,
            drop_effect: register_format(w!("Preferred DropEffect"))?,
        })
    }) {
        Ok(formats) => Ok(formats),
        Err(_) => Err(AppError::Windows("无法注册剪贴板格式".to_owned())),
    }
}

fn register_format(name: PCWSTR) -> Result<u16, u32> {
    // SAFETY: name is a string literal PCWSTR that lives for the process.
    let id = unsafe { RegisterClipboardFormatW(name) };
    u16::try_from(id)
        .map_err(|_| id)
        .and_then(|id| (id != 0).then_some(id).ok_or(0))
}

/// 把面板快照编码成顶向下的 `CF_DIBV5` 数据（`BITMAPV5HEADER` + 像素）。
///
/// 缓冲里的 alpha 是预乘的，这里转成直通后再交给 `CF_DIBV5`/`CF_DIB`：只认
/// `CF_DIB` 的接收方会把 RGB 直接当原色用。面板截图本身已经铺成不透明，转换
/// 对那些像素是 no-op；半透明夹具仍需除回。
fn dibv5_bytes(bitmap: &PanelBitmap) -> Result<Vec<u8>, AppError> {
    let header = BITMAPV5HEADER {
        bV5Size: u32::try_from(size_of::<BITMAPV5HEADER>())
            .map_err(|_| AppError::Render("位图头大小溢出".to_owned()))?,
        bV5Width: bitmap.width,
        // 负高度 = 顶向下，与快照的行序一致，接收方不会按自下而上翻转。
        bV5Height: -bitmap.height,
        bV5Planes: 1,
        bV5BitCount: 32,
        bV5Compression: BI_BITFIELDS,
        bV5SizeImage: u32::try_from(bitmap.pixels.len())
            .map_err(|_| AppError::Render("位图数据大小溢出".to_owned()))?,
        bV5RedMask: 0x00ff_0000,
        bV5GreenMask: 0x0000_ff00,
        bV5BlueMask: 0x0000_00ff,
        bV5AlphaMask: 0xff00_0000,
        bV5CSType: LCS_SRGB,
        ..Default::default()
    };
    // SAFETY: the header is a plain-integer POD struct, so viewing it as exactly
    // size_of::<BITMAPV5HEADER>() bytes is valid (and it is #[repr(C)], no padding).
    let header_bytes = unsafe {
        std::slice::from_raw_parts(
            ptr::from_ref(&header).cast::<u8>(),
            size_of::<BITMAPV5HEADER>(),
        )
    };
    let mut bytes = Vec::with_capacity(header_bytes.len() + bitmap.pixels.len());
    bytes.extend_from_slice(header_bytes);
    bytes.extend_from_slice(&to_straight_alpha(&bitmap.pixels));
    Ok(bytes)
}

/// 顶向下的 32 位 `CF_DIB`（`BITMAPINFOHEADER` + BGRA），给不认 `CF_DIBV5` 的接收方。
fn dib_bytes(bitmap: &PanelBitmap) -> Result<Vec<u8>, AppError> {
    let pixels = to_straight_alpha(&bitmap.pixels);
    let header = BITMAPINFOHEADER {
        biSize: u32::try_from(size_of::<BITMAPINFOHEADER>())
            .map_err(|_| AppError::Render("位图头大小溢出".to_owned()))?,
        biWidth: bitmap.width,
        biHeight: -bitmap.height,
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB.0,
        biSizeImage: u32::try_from(pixels.len())
            .map_err(|_| AppError::Render("位图数据大小溢出".to_owned()))?,
        ..Default::default()
    };
    // SAFETY: BITMAPINFOHEADER is a plain-integer POD with no padding.
    let header_bytes = unsafe {
        std::slice::from_raw_parts(
            ptr::from_ref(&header).cast::<u8>(),
            size_of::<BITMAPINFOHEADER>(),
        )
    };
    let mut bytes = Vec::with_capacity(header_bytes.len() + pixels.len());
    bytes.extend_from_slice(header_bytes);
    bytes.extend_from_slice(&pixels);
    Ok(bytes)
}

/// 预乘 BGRA 转直通（非预乘）BGRA。
///
/// 预乘下每个通道 ≤ alpha，除回去就还原原色；alpha 为 0 的像素没有可还原的颜色
/// （面板圆角外），保持 0。alpha 为 255 时预乘与直通完全等价，直接跳过。
fn to_straight_alpha(premultiplied: &[u8]) -> Vec<u8> {
    let mut bytes = premultiplied.to_vec();
    for pixel in bytes.chunks_exact_mut(4) {
        let alpha = u32::from(pixel[3]);
        if alpha == 0 || alpha == 255 {
            continue;
        }
        for channel in &mut pixel[..3] {
            // 四舍五入的整数除法，避免整幅图整体偏暗。
            let straight = (u32::from(*channel) * 255 + alpha / 2) / alpha;
            *channel = u8::try_from(straight.min(255)).unwrap_or(u8::MAX);
        }
    }
    bytes
}

/// 快照应有的像素字节数；同时用作写入前的自检，避免头里声明的尺寸与缓冲不一致
/// ——那会让接收方按错误的长度读取这块全局内存。
fn pixel_bytes(width: i32, height: i32) -> Option<usize> {
    usize::try_from(width)
        .ok()
        .zip(usize::try_from(height).ok())
        .and_then(|(width, height)| width.checked_mul(height))
        .and_then(|pixels| pixels.checked_mul(4))
}

/// 用系统 WIC 把快照编成 PNG 文件字节。
fn encode_png(bitmap: &PanelBitmap) -> Result<Vec<u8>, AppError> {
    let width =
        u32::try_from(bitmap.width).map_err(|_| AppError::Render("截图宽度无效".to_owned()))?;
    let height =
        u32::try_from(bitmap.height).map_err(|_| AppError::Render("截图高度无效".to_owned()))?;
    let pixels = to_straight_alpha(&bitmap.pixels);
    let stride = width
        .checked_mul(4)
        .ok_or_else(|| AppError::Render("截图行宽溢出".to_owned()))?;
    if usize::try_from(stride)
        .ok()
        .and_then(|stride| usize::try_from(height).ok().map(|height| stride * height))
        != Some(pixels.len())
    {
        return Err(AppError::Render("截图像素与尺寸不一致".to_owned()));
    }

    let factory: IWICImagingFactory =
        unsafe { CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER) }?;
    let stream = unsafe { CreateStreamOnHGlobal(HGLOBAL::default(), true) }?;
    let encoder = unsafe { factory.CreateEncoder(&GUID_ContainerFormatPng, ptr::null()) }?;
    unsafe { encoder.Initialize(&stream, WICBitmapEncoderNoCache) }?;

    let mut frame: Option<IWICBitmapFrameEncode> = None;
    let mut options = None;
    unsafe { encoder.CreateNewFrame(&mut frame, &mut options) }?;
    drop(options);
    let frame = frame.ok_or_else(|| AppError::Render("无法创建 PNG 帧".to_owned()))?;
    unsafe { frame.Initialize(None) }?;
    unsafe { frame.SetSize(width, height) }?;
    let mut format = GUID_WICPixelFormat32bppBGRA;
    unsafe { frame.SetPixelFormat(&mut format) }?;
    if format != GUID_WICPixelFormat32bppBGRA {
        return Err(AppError::Render("PNG 编码器不接受 BGRA".to_owned()));
    }
    unsafe { frame.WritePixels(height, stride, &pixels) }?;
    unsafe { frame.Commit() }?;
    unsafe { encoder.Commit() }?;

    let mut stat = STATSTG::default();
    unsafe { stream.Stat(&mut stat, STATFLAG_NONAME) }?;
    let size = usize::try_from(stat.cbSize).map_err(|_| AppError::Render("PNG 过大".to_owned()))?;
    let hg = unsafe { GetHGlobalFromStream(&stream) }?;
    if size == 0 || size > unsafe { GlobalSize(hg) } {
        return Err(AppError::Render("PNG 编码长度无效".to_owned()));
    }
    let locked = unsafe { GlobalLock(hg) };
    if locked.is_null() {
        return Err(AppError::Render("无法读取 PNG 编码结果".to_owned()));
    }
    let bytes = unsafe { std::slice::from_raw_parts(locked.cast::<u8>(), size) }.to_vec();
    let _ = unsafe { GlobalUnlock(hg) };
    Ok(bytes)
}

fn file_group_descriptor(png_len: u32) -> Result<Vec<u8>, AppError> {
    let mut name: Vec<u16> = SNAPSHOT_FILE_NAME.encode_utf16().chain(Some(0)).collect();
    if name.len() > 260 {
        return Err(AppError::Render("截图文件名过长".to_owned()));
    }

    let mut descriptor = FILEGROUPDESCRIPTORW {
        cItems: 1,
        ..Default::default()
    };
    // FILEDESCRIPTORW 是 packed(1)，不能安全地取字段引用，只能按地址写。
    unsafe {
        let file = ptr::addr_of_mut!(descriptor.fgd[0]);
        ptr::write(
            ptr::addr_of_mut!((*file).dwFlags),
            (FD_ATTRIBUTES.0 | FD_FILESIZE.0) as u32,
        );
        ptr::write(
            ptr::addr_of_mut!((*file).dwFileAttributes),
            FILE_ATTRIBUTE_NORMAL.0,
        );
        ptr::write(ptr::addr_of_mut!((*file).nFileSizeLow), png_len);
        ptr::copy_nonoverlapping(
            name.as_mut_ptr(),
            ptr::addr_of_mut!((*file).cFileName).cast::<u16>(),
            name.len(),
        );
    }

    Ok(unsafe {
        std::slice::from_raw_parts(
            ptr::from_ref(&descriptor).cast::<u8>(),
            size_of::<FILEGROUPDESCRIPTORW>(),
        )
    }
    .to_vec())
}

fn formatetc(cf: u16, tymed: i32, lindex: i32) -> FORMATETC {
    FORMATETC {
        cfFormat: cf,
        ptd: ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex,
        tymed: tymed as u32,
    }
}

fn stgmedium_hglobal(bytes: &[u8]) -> Result<STGMEDIUM, WinError> {
    let memory = MoveableMemory::alloc(bytes.len())
        .and_then(|memory| {
            memory.write(bytes)?;
            Ok(memory)
        })
        .map_err(|_| WinError::from_hresult(E_OUTOFMEMORY))?;
    let handle = memory.0;
    std::mem::forget(memory);
    Ok(STGMEDIUM {
        tymed: TYMED_HGLOBAL.0 as u32,
        u: STGMEDIUM_0 { hGlobal: handle },
        pUnkForRelease: ManuallyDrop::new(None),
    })
}

fn stgmedium_gdi_bitmap(dib: &[u8]) -> Result<STGMEDIUM, WinError> {
    if dib.len() < DIB_HEADER_LEN {
        return Err(WinError::from_hresult(E_INVALIDARG));
    }
    let header = unsafe { ptr::read_unaligned(dib.as_ptr().cast::<BITMAPINFOHEADER>()) };
    let pixel_offset = usize::try_from(header.biSize).unwrap_or(0);
    if pixel_offset < DIB_HEADER_LEN || dib.len() < pixel_offset {
        return Err(WinError::from_hresult(E_INVALIDARG));
    }
    let pixels = &dib[pixel_offset..];
    let info = BITMAPINFO {
        bmiHeader: header,
        bmiColors: [Default::default()],
    };
    let mut bits = ptr::null_mut();
    let bitmap = unsafe { CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0) }
        .map_err(|_| WinError::from_hresult(E_OUTOFMEMORY))?;
    if bits.is_null() {
        let _ = unsafe { DeleteObject(bitmap.into()) };
        return Err(WinError::from_hresult(E_OUTOFMEMORY));
    }
    unsafe { ptr::copy_nonoverlapping(pixels.as_ptr(), bits.cast::<u8>(), pixels.len()) };
    Ok(STGMEDIUM {
        tymed: TYMED_GDI.0 as u32,
        u: STGMEDIUM_0 { hBitmap: bitmap },
        pUnkForRelease: ManuallyDrop::new(None),
    })
}

fn stgmedium_istream(bytes: &[u8]) -> Result<STGMEDIUM, WinError> {
    let stream = unsafe { SHCreateMemStream(Some(bytes)) }
        .ok_or_else(|| WinError::from_hresult(E_OUTOFMEMORY))?;
    Ok(STGMEDIUM {
        tymed: TYMED_ISTREAM.0 as u32,
        u: STGMEDIUM_0 {
            pstm: ManuallyDrop::new(Some(stream)),
        },
        pUnkForRelease: ManuallyDrop::new(None),
    })
}

#[derive(Clone, Copy)]
enum ClipboardOffer {
    Dibv5,
    Dib,
    Bitmap,
    Png,
    FileDescriptor,
    FileContents,
    DropEffect,
}

/// 剪贴板格式表：`(cfFormat, tymed, lindex, 被请求时的应答)`。
///
/// 枚举（`EnumFormatEtc`）与匹配（`GetData`/`QueryGetData`）共用同一份，避免两张表
/// 各写一遍之后 tymed/lindex 对不上——那会让我们答得出 `GetData` 却在枚举里看不见。
type OfferTable = [(u16, i32, i32, ClipboardOffer); 7];

fn offers() -> Result<OfferTable, WinError> {
    let formats = clipboard_formats().map_err(|_| WinError::from_hresult(E_INVALIDARG))?;
    Ok([
        (formats.png, TYMED_HGLOBAL.0, -1, ClipboardOffer::Png),
        (
            formats.file_descriptor,
            TYMED_HGLOBAL.0,
            -1,
            ClipboardOffer::FileDescriptor,
        ),
        (
            formats.file_contents,
            TYMED_ISTREAM.0 | TYMED_HGLOBAL.0,
            0,
            ClipboardOffer::FileContents,
        ),
        (CF_DIBV5 as u16, TYMED_HGLOBAL.0, -1, ClipboardOffer::Dibv5),
        (CF_DIB, TYMED_HGLOBAL.0, -1, ClipboardOffer::Dib),
        (CF_BITMAP, TYMED_GDI.0, -1, ClipboardOffer::Bitmap),
        (
            formats.drop_effect,
            TYMED_HGLOBAL.0,
            -1,
            ClipboardOffer::DropEffect,
        ),
    ])
}

/// 剪贴板上的面板快照：位图给聊天框，虚拟 PNG 文件给资源管理器。
#[implement(IDataObject)]
struct SnapshotData {
    dibv5: Vec<u8>,
    dib: Vec<u8>,
    png: Vec<u8>,
    descriptor: Vec<u8>,
}

impl SnapshotData {
    fn offered_formats() -> Result<[FORMATETC; 7], WinError> {
        Ok(offers()?.map(|(cf, tymed, lindex, _)| formatetc(cf, tymed, lindex)))
    }

    fn match_offer(requested: &FORMATETC) -> Result<ClipboardOffer, WinError> {
        let offers = offers().map_err(|_| WinError::from_hresult(DV_E_FORMATETC))?;
        if requested.ptd.is_null()
            && (requested.dwAspect == 0 || requested.dwAspect == DVASPECT_CONTENT.0)
        {
            for (cf, tymed, lindex, offer) in offers {
                if requested.cfFormat == cf
                    && requested.tymed & tymed as u32 != 0
                    && (requested.lindex == lindex || requested.lindex == -1)
                {
                    return Ok(offer);
                }
            }
        }
        Err(WinError::from_hresult(DV_E_FORMATETC))
    }

    fn medium_for(&self, requested: &FORMATETC) -> Result<STGMEDIUM, WinError> {
        match Self::match_offer(requested)? {
            ClipboardOffer::Dibv5 => stgmedium_hglobal(&self.dibv5),
            ClipboardOffer::Dib => stgmedium_hglobal(&self.dib),
            ClipboardOffer::Bitmap => stgmedium_gdi_bitmap(&self.dib),
            ClipboardOffer::Png => stgmedium_hglobal(&self.png),
            ClipboardOffer::FileDescriptor => stgmedium_hglobal(&self.descriptor),
            ClipboardOffer::FileContents => {
                if requested.tymed & TYMED_ISTREAM.0 as u32 != 0 {
                    stgmedium_istream(&self.png)
                } else {
                    stgmedium_hglobal(&self.png)
                }
            }
            ClipboardOffer::DropEffect => stgmedium_hglobal(&DROPEFFECT_COPY.0.to_le_bytes()),
        }
    }
}

impl IDataObject_Impl for SnapshotData_Impl {
    fn GetData(&self, pformatetcin: *const FORMATETC) -> windows::core::Result<STGMEDIUM> {
        let requested =
            unsafe { pformatetcin.as_ref() }.ok_or_else(|| WinError::from_hresult(E_INVALIDARG))?;
        self.medium_for(requested)
    }

    fn GetDataHere(
        &self,
        _pformatetc: *const FORMATETC,
        _pmedium: *mut STGMEDIUM,
    ) -> windows::core::Result<()> {
        Err(WinError::from_hresult(E_NOTIMPL))
    }

    fn QueryGetData(&self, pformatetc: *const FORMATETC) -> HRESULT {
        match unsafe { pformatetc.as_ref() } {
            Some(requested) => {
                SnapshotData::match_offer(requested).map_or(DV_E_FORMATETC, |_| S_OK)
            }
            None => E_INVALIDARG,
        }
    }

    fn GetCanonicalFormatEtc(
        &self,
        pformatectin: *const FORMATETC,
        pformatetcout: *mut FORMATETC,
    ) -> HRESULT {
        let Some(requested) = (unsafe { pformatectin.as_ref() }) else {
            return E_INVALIDARG;
        };
        let Some(output) = (unsafe { pformatetcout.as_mut() }) else {
            return E_INVALIDARG;
        };
        *output = *requested;
        output.ptd = ptr::null_mut();
        DATA_S_SAMEFORMATETC
    }

    fn SetData(
        &self,
        _pformatetc: *const FORMATETC,
        _pmedium: *const STGMEDIUM,
        _frelease: BOOL,
    ) -> windows::core::Result<()> {
        Err(WinError::from_hresult(E_NOTIMPL))
    }

    fn EnumFormatEtc(&self, dwdirection: u32) -> windows::core::Result<IEnumFORMATETC> {
        if dwdirection != DATADIR_GET.0 as u32 {
            return Err(WinError::from_hresult(E_NOTIMPL));
        }
        let formats = SnapshotData::offered_formats()?;
        unsafe { SHCreateStdEnumFmtEtc(&formats) }
    }

    fn DAdvise(
        &self,
        _pformatetc: *const FORMATETC,
        _advf: u32,
        _padvsink: Ref<'_, IAdviseSink>,
    ) -> windows::core::Result<u32> {
        Err(WinError::from_hresult(OLE_E_ADVISENOTSUPPORTED))
    }

    fn DUnadvise(&self, _dwconnection: u32) -> windows::core::Result<()> {
        Err(WinError::from_hresult(OLE_E_ADVISENOTSUPPORTED))
    }

    fn EnumDAdvise(&self) -> windows::core::Result<IEnumSTATDATA> {
        Err(WinError::from_hresult(OLE_E_ADVISENOTSUPPORTED))
    }
}

/// 把面板快照写进剪贴板：位图 + PNG 虚拟文件。
///
/// 编码都在 `OleSetClipboard` 之前做完，避免清空别人剪贴板之后再失败。
/// OLE 未初始化时 `OleSetClipboard` 失败，由调用方提示重试。
pub(super) fn copy_panel_bitmap(bitmap: &PanelBitmap) -> Result<(), AppError> {
    if pixel_bytes(bitmap.width, bitmap.height) != Some(bitmap.pixels.len()) {
        return Err(AppError::Render("面板截图尺寸与像素缓冲不一致".to_owned()));
    }
    let _ = clipboard_formats()?;
    let dibv5 = dibv5_bytes(bitmap)?;
    let dib = dib_bytes(bitmap)?;
    let png = encode_png(bitmap)?;
    let png_len = u32::try_from(png.len()).map_err(|_| AppError::Render("PNG 过大".to_owned()))?;
    let descriptor = file_group_descriptor(png_len)?;
    let object: IDataObject = SnapshotData {
        dibv5,
        dib,
        png,
        descriptor,
    }
    .into();
    // SAFETY: OLE 已在 UI 线程初始化；对象在调用成功后由剪贴板 AddRef。
    unsafe { OleSetClipboard(&object) }?;
    Ok(())
}

/// 剪贴板打开守卫：任何提前返回都会关闭它。
#[cfg(test)]
struct ClipboardGuard;

#[cfg(test)]
impl ClipboardGuard {
    fn open(owner: HWND) -> Result<Self, AppError> {
        // SAFETY: owner is the live UI-thread window; Drop closes what this call opens.
        unsafe { OpenClipboard(Some(owner)) }
            .map_err(|_| AppError::Windows("剪贴板正被其他程序占用，请稍后重试".to_owned()))?;
        Ok(Self)
    }
}

#[cfg(test)]
impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        // SAFETY: balances the OpenClipboard that created this guard.
        let _ = unsafe { CloseClipboard() };
    }
}

/// `GMEM_MOVEABLE` 全局内存块：交给系统之前，释放由这里负责。
struct MoveableMemory(HGLOBAL);

impl MoveableMemory {
    fn alloc(len: usize) -> Result<Self, AppError> {
        // SAFETY: no borrowed input; ownership stays here until the system takes it.
        unsafe { GlobalAlloc(GMEM_MOVEABLE, len) }
            .map(Self)
            .map_err(|_| AppError::Windows("无法为剪贴板位图分配内存".to_owned()))
    }

    fn write(&self, bytes: &[u8]) -> Result<(), AppError> {
        // SAFETY: the block is live and was allocated with at least bytes.len() bytes.
        let destination = unsafe { GlobalLock(self.0) };
        if destination.is_null() {
            return Err(AppError::Windows("无法锁定剪贴板位图内存".to_owned()));
        }
        // SAFETY: destination covers bytes.len() writable bytes inside this block.
        unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), destination.cast::<u8>(), bytes.len()) };
        // GlobalUnlock 在引用计数归零（也就是这一次成功解锁）时同样返回 0，windows
        // crate 把 0 映射成 Err，因此只能忽略返回值——真错误要另看 GetLastError。
        // SAFETY: balances the GlobalLock above on the same block.
        let _ = unsafe { GlobalUnlock(self.0) };
        Ok(())
    }
}

impl Drop for MoveableMemory {
    fn drop(&mut self) {
        // SAFETY: the block is still owned here (the system never took it). GlobalFree
        // reports success as a null return, which the binding maps to Err, hence the ignore.
        let _ = unsafe { GlobalFree(Some(self.0)) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `BITMAPV5HEADER` 的实际字节长度：124。`bV5Planes` 与 `bV5BitCount` 两个
    /// u16 正好填满首三个 u32 之后的对齐空隙，所以没有尾部填充。
    const HEADER_LEN: usize = 124;

    fn snapshot(width: i32, height: i32, pixels: Vec<u8>) -> PanelBitmap {
        PanelBitmap {
            width,
            height,
            pixels,
        }
    }

    fn header_u16(bytes: &[u8], offset: usize) -> u16 {
        u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
    }

    fn header_u32(bytes: &[u8], offset: usize) -> u32 {
        u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
    }

    fn header_i32(bytes: &[u8], offset: usize) -> i32 {
        i32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
    }

    #[test]
    fn dibv5_header_describes_a_top_down_32bit_srgb_bitmap() {
        let bytes = dibv5_bytes(&snapshot(3, 2, vec![0; 24])).unwrap();

        assert_eq!(bytes.len(), HEADER_LEN + 24);
        assert_eq!(
            header_u32(&bytes, 0),
            u32::try_from(size_of::<BITMAPV5HEADER>()).unwrap(),
            "bV5Size 必须等于结构体自身长度"
        );
        assert_eq!(header_i32(&bytes, 4), 3, "bV5Width");
        assert_eq!(header_i32(&bytes, 8), -2, "bV5Height 为负表示顶向下");
        assert_eq!(header_u16(&bytes, 12), 1, "bV5Planes");
        assert_eq!(header_u16(&bytes, 14), 32, "bV5BitCount");
        assert_eq!(
            header_u32(&bytes, 16),
            BI_BITFIELDS.0,
            "bV5Compression 必须是 BI_BITFIELDS，否则掩码无效"
        );
        assert_eq!(header_u32(&bytes, 20), 24, "bV5SizeImage");
        assert_eq!(header_u32(&bytes, 40), 0x00ff_0000, "bV5RedMask");
        assert_eq!(header_u32(&bytes, 44), 0x0000_ff00, "bV5GreenMask");
        assert_eq!(header_u32(&bytes, 48), 0x0000_00ff, "bV5BlueMask");
        assert_eq!(header_u32(&bytes, 52), 0xff00_0000, "bV5AlphaMask");
        assert_eq!(
            header_u32(&bytes, 56),
            LCS_SRGB,
            "bV5CSType 必须声明 sRGB，否则系统会按校准 RGB 再转一次色彩空间"
        );
    }

    #[test]
    fn dib_header_describes_a_top_down_32bit_bitmap() {
        let bytes = dib_bytes(&snapshot(3, 2, vec![0; 24])).unwrap();
        assert_eq!(bytes.len(), DIB_HEADER_LEN + 24);
        assert_eq!(header_u32(&bytes, 0), 40, "biSize");
        assert_eq!(header_i32(&bytes, 4), 3, "biWidth");
        assert_eq!(header_i32(&bytes, 8), -2, "biHeight 为负表示顶向下");
        assert_eq!(header_u16(&bytes, 12), 1, "biPlanes");
        assert_eq!(header_u16(&bytes, 14), 32, "biBitCount");
        assert_eq!(header_u32(&bytes, 16), BI_RGB.0, "biCompression 为 BI_RGB");
        assert_eq!(&bytes[DIB_HEADER_LEN..], &[0; 24]);
    }

    #[test]
    fn dib_header_is_followed_by_pixels_without_flipping_rows() {
        let pixels = vec![
            1, 2, 3, 0, 4, 5, 6, 255, // 顶行
            7, 8, 9, 0, 10, 11, 12, 255, // 底行
        ];
        let bytes = dibv5_bytes(&snapshot(2, 2, pixels.clone())).unwrap();

        assert_eq!(&bytes[HEADER_LEN..], &to_straight_alpha(&pixels)[..]);
        assert_eq!(
            &bytes[HEADER_LEN..HEADER_LEN + 4],
            &[1, 2, 3, 0],
            "顶向下的缓冲里第一行像素要紧跟头部"
        );
    }

    #[test]
    fn premultiplied_pixels_become_straight_alpha() {
        // 全透明：没有可还原的颜色，保持 0
        assert_eq!(to_straight_alpha(&[0, 0, 0, 0]), vec![0, 0, 0, 0]);
        // 半透明：预乘值除回 alpha = 128/255
        assert_eq!(
            to_straight_alpha(&[128, 64, 32, 128]),
            vec![255, 128, 64, 128]
        );
        // 完全不透明：预乘与直通等价
        assert_eq!(
            to_straight_alpha(&[255, 128, 64, 255]),
            vec![255, 128, 64, 255]
        );
    }

    #[test]
    fn straight_alpha_stays_monotonic_and_never_overflows_a_channel() {
        for alpha in 1..=255u8 {
            let mut previous = 0u8;
            for channel in 0..=alpha {
                let straight = to_straight_alpha(&[channel, 0, 0, alpha])[0];
                assert!(
                    straight >= previous,
                    "alpha={alpha} channel={channel} 应随原色单调不减"
                );
                assert_eq!(
                    straight == 255,
                    channel == alpha,
                    "alpha={alpha} channel={channel} 只有原色满值才能到 255"
                );
                previous = straight;
            }
        }
    }

    #[test]
    fn snapshots_whose_buffer_does_not_match_their_size_are_rejected() {
        // 尺寸与缓冲不一致时宁可不写剪贴板，否则接收方会按错长度读全局内存。
        assert!(pixel_bytes(2, 2) == Some(16));
        assert_ne!(pixel_bytes(2, 2), Some(12));
        assert_eq!(pixel_bytes(-2, 2), None);
    }

    #[test]
    fn file_group_descriptor_names_the_png() {
        let bytes = file_group_descriptor(1234).unwrap();
        assert_eq!(header_u32(&bytes, 0), 1, "cItems 应为 1 个文件");
        let name: Vec<u16> = SNAPSHOT_FILE_NAME.encode_utf16().chain(Some(0)).collect();
        let name_bytes: Vec<u8> = name.iter().flat_map(|unit| unit.to_le_bytes()).collect();
        assert!(
            bytes
                .windows(name_bytes.len())
                .any(|window| window == name_bytes),
            "描述符应包含文件名 CodexQuota.png"
        );
        // nFileSizeLow 在 FILEDESCRIPTORW 里紧挨文件名之前：4+16+8+8+4+8+8+8+4 = 68，
        // 再加上 FILEGROUPDESCRIPTORW.cItems 的 4 字节。
        assert_eq!(header_u32(&bytes, 72), 1234, "nFileSizeLow");
    }

    #[test]
    fn encode_png_writes_a_png_with_matching_dimensions() {
        use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};

        // WIC 需要 COM；S_FALSE（已初始化）也是成功。
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.unwrap();

        let bitmap = snapshot(2, 1, vec![0x20, 0x17, 0x12, 255, 0x20, 0x17, 0x12, 255]);
        let png = encode_png(&bitmap).expect("编码 PNG");
        assert!(
            png.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]),
            "应有 PNG 签名"
        );
        assert_eq!(&png[12..16], b"IHDR");
        assert_eq!(u32::from_be_bytes(png[16..20].try_into().unwrap()), 2);
        assert_eq!(u32::from_be_bytes(png[20..24].try_into().unwrap()), 1);
        assert!(
            png.ends_with(&[0, 0, 0, 0, b'I', b'E', b'N', b'D', 0xAE, 0x42, 0x60, 0x82]),
            "长度必须是 IStream 逻辑大小，以 IEND 结束，不能带上 GlobalSize 的堆块尾巴"
        );
    }

    /// 真实剪贴板往返：把渲染出的面板写进去再读回来，逐字节比对，并确认 `CF_DIB`
    /// / `CF_BITMAP` 作为显式格式可取——OLE 延迟渲染不会走系统合成。
    ///
    /// **会覆盖当前剪贴板内容**，所以平时跳过；需要时手动运行：
    /// `cargo test real_clipboard -- --ignored`
    #[test]
    #[ignore = "会覆盖剪贴板内容"]
    fn real_clipboard_round_trip_keeps_the_dib_readable_and_converts_to_cf_dib() {
        use crate::win32::AppState;
        use crate::win32::PANEL_WIDTH_DIP;
        use crate::win32::layout::dip_to_px;
        use crate::win32::presentation::panel_height_dip;
        use crate::win32::renderer::Renderer;
        use windows::Win32::Graphics::Gdi::{BITMAP, GetObjectW, HGDIOBJ};
        use windows::Win32::System::DataExchange::{GetClipboardData, IsClipboardFormatAvailable};
        use windows::Win32::System::Ole::OleInitialize;

        // `CF_DIB` / `CF_BITMAP` 由数据对象显式提供，不再依赖系统合成。
        unsafe { OleInitialize(None) }.unwrap();

        let state = AppState::default();
        let width = dip_to_px(PANEL_WIDTH_DIP, 96);
        let height = dip_to_px(panel_height_dip(&state), 96);
        let mut renderer = Renderer::new(width, height, 96).unwrap();
        let bitmap = renderer.panel_snapshot(&state).unwrap();

        let owner = create_owner_window();
        copy_panel_bitmap(&bitmap).unwrap();

        let clipboard_guard = ClipboardGuard::open(owner).unwrap();
        let formats = clipboard_formats().unwrap();
        assert!(unsafe { IsClipboardFormatAvailable(CF_DIBV5) }.is_ok());
        assert!(
            unsafe { IsClipboardFormatAvailable(u32::from(CF_DIB)) }.is_ok(),
            "应显式提供 CF_DIB"
        );
        assert!(
            unsafe { IsClipboardFormatAvailable(u32::from(CF_BITMAP)) }.is_ok(),
            "应显式提供 CF_BITMAP"
        );
        assert!(
            unsafe { IsClipboardFormatAvailable(u32::from(formats.png)) }.is_ok(),
            "应提供 PNG 格式"
        );
        assert!(
            unsafe { IsClipboardFormatAvailable(u32::from(formats.file_descriptor)) }.is_ok(),
            "应提供 FileGroupDescriptorW，供资源管理器粘贴成文件"
        );

        // 我们自己写进去的那一份：头部与像素都逐字节可预期。
        {
            let handle = unsafe { GetClipboardData(CF_DIBV5) }.expect("读取 CF_DIBV5");
            // 取回的句柄仍归剪贴板所有，只能读和解锁，不能释放。
            let block = HGLOBAL(handle.0);
            let base = unsafe { GlobalLock(block) }.cast::<u8>();
            assert!(!base.is_null(), "CF_DIBV5 的内存无法锁定");

            // SAFETY: the producer wrote a BITMAPV5HEADER followed by width * height * 4 bytes.
            let header = unsafe { std::slice::from_raw_parts(base, size_of::<BITMAPV5HEADER>()) };
            assert_eq!(header_i32(header, 4), width, "CF_DIBV5 的宽度");
            assert_eq!(header_i32(header, 8), -height, "CF_DIBV5 应为顶向下");
            assert_eq!(header_u32(header, 56), LCS_SRGB, "CF_DIBV5 的色彩空间");

            // SAFETY: the pixel bytes follow the header and cover the whole snapshot.
            let pixels = unsafe {
                std::slice::from_raw_parts(
                    base.add(size_of::<BITMAPV5HEADER>()),
                    bitmap.pixels.len(),
                )
            };
            assert_eq!(
                pixels,
                &to_straight_alpha(&bitmap.pixels)[..],
                "CF_DIBV5 像素"
            );
            let _ = unsafe { GlobalUnlock(block) };
        }

        {
            let handle = unsafe { GetClipboardData(u32::from(CF_DIB)) }.expect("读取 CF_DIB");
            let block = HGLOBAL(handle.0);
            let base = unsafe { GlobalLock(block) }.cast::<u8>();
            assert!(!base.is_null(), "CF_DIB 的内存无法锁定");

            // SAFETY: we write a 40-byte BITMAPINFOHEADER followed by the snapshot pixels.
            let header = unsafe { std::slice::from_raw_parts(base, DIB_HEADER_LEN) };
            assert_eq!(header_u32(header, 0), 40, "CF_DIB 应为 BITMAPINFOHEADER");
            assert_eq!(header_i32(header, 4), width, "CF_DIB 的宽度");
            assert_eq!(header_i32(header, 8), -height, "CF_DIB 应为顶向下");
            assert_eq!(header_u16(header, 14), 32, "CF_DIB 的位深");
            let pixels = unsafe {
                std::slice::from_raw_parts(base.add(DIB_HEADER_LEN), bitmap.pixels.len())
            };
            assert_eq!(
                pixels,
                &to_straight_alpha(&bitmap.pixels)[..],
                "CF_DIB 像素"
            );
            let _ = unsafe { GlobalUnlock(block) };
        }

        // `CF_BITMAP` 走的是另一条构造路径（DIB 字节 -> CreateDIBSection -> HBITMAP），
        // 这里真正取回句柄查一次几何，别只确认"格式在"。
        {
            let handle = unsafe { GetClipboardData(u32::from(CF_BITMAP)) }.expect("读取 CF_BITMAP");
            let mut info = BITMAP::default();
            let expected = i32::try_from(size_of::<BITMAP>()).unwrap();
            let copied =
                unsafe { GetObjectW(HGDIOBJ(handle.0), expected, Some(ptr::from_mut(&mut info).cast())) };
            assert_eq!(copied, expected, "CF_BITMAP 应是一只可查询的位图");
            assert_eq!(info.bmWidth, width, "CF_BITMAP 的宽度");
            // 顶向下 DIB section 的高度符号由实现决定，只比绝对值。
            assert_eq!(info.bmHeight.abs(), height, "CF_BITMAP 的高度");
            assert_eq!(info.bmBitsPixel, 32, "CF_BITMAP 的位深");
        }

        drop(clipboard_guard);
        // SAFETY: owner was created by create_owner_window and is not used afterwards.
        let _ = unsafe { windows::Win32::UI::WindowsAndMessaging::DestroyWindow(owner) };
    }

    /// 真实窗口当剪贴板 owner：`CreateWindowExW` 直接用系统已注册的 STATIC 类，
    /// 不用自己注册窗口类。
    fn create_owner_window() -> HWND {
        // SAFETY: a system-registered class with no parent creates a hidden window owned
        // by this thread; it is destroyed at the end of the test.
        unsafe {
            windows::Win32::UI::WindowsAndMessaging::CreateWindowExW(
                Default::default(),
                windows::core::w!("STATIC"),
                windows::core::w!(""),
                windows::Win32::UI::WindowsAndMessaging::WS_POPUP,
                0,
                0,
                0,
                0,
                None,
                None,
                None,
                None,
            )
        }
        .expect("创建剪贴板 owner 窗口")
    }
}
