use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDIBits,
    HBITMAP, HDC, SelectObject,
};
use windows::Win32::System::Com::IBindCtx;
use windows::Win32::UI::Shell::{IShellItem, IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_ICONONLY};
use windows::core::{Interface, PCWSTR};

fn hbitmap_to_bgra_premul(hdc: HDC, bitmap: HBITMAP, size: usize) -> Option<Vec<u8>> {
    let total_bytes = size * size * 4;
    let mut buf = vec![0u8; total_bytes];

    let mut bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: size as i32,
            biHeight: -(size as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };

    let result = unsafe {
        GetDIBits(
            hdc,
            bitmap,
            0,
            size as u32,
            Some(buf.as_mut_ptr() as *mut std::ffi::c_void),
            &mut bmi,
            DIB_RGB_COLORS,
        )
    };
    if result == 0 {
        return None;
    }

    let mut pm = Vec::with_capacity(total_bytes);
    for px in buf.chunks_exact(4) {
        let (b, g, r, a) = (px[0], px[1], px[2], px[3]);
        let a = a as f32 / 255.0;
        pm.push((b as f32 * a).round() as u8);
        pm.push((g as f32 * a).round() as u8);
        pm.push((r as f32 * a).round() as u8);
        pm.push(px[3]);
    }
    Some(pm)
}

fn extract_from_factory(factory: &IShellItemImageFactory, size: usize) -> Option<Vec<u8>> {
    let size_pt = windows::Win32::Foundation::SIZE {
        cx: size as i32,
        cy: size as i32,
    };
    let hbitmap = unsafe { factory.GetImage(size_pt, SIIGBF_ICONONLY).ok() }?;
    let hdc = unsafe { CreateCompatibleDC(None) };
    if hdc.0.is_null() {
        unsafe {
            let _ = DeleteObject(hbitmap);
        }
        return None;
    }
    let old = unsafe { SelectObject(hdc, hbitmap) };
    let result = hbitmap_to_bgra_premul(hdc, hbitmap, size);
    unsafe {
        let _ = SelectObject(hdc, old);
        let _ = DeleteObject(hbitmap);
        let _ = DeleteDC(hdc);
    }
    result
}

fn try_shell_item(item: &IShellItem, size: usize) -> Option<Vec<u8>> {
    let factory: IShellItemImageFactory = item.cast().ok()?;
    extract_from_factory(&factory, size)
}

fn try_parsing_name(path: &str, size: usize) -> Option<Vec<u8>> {
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    let pcwstr = PCWSTR(wide.as_ptr());
    let item: IShellItem = unsafe { SHCreateItemFromParsingName(pcwstr, Option::<&IBindCtx>::None).ok() }?;
    try_shell_item(&item, size)
}

fn extract_from_aumid(aumid: &str, size: usize) -> Option<Vec<u8>> {
    let apps_path = format!("shell:AppsFolder\\{}", aumid);
    try_parsing_name(&apps_path, size)
}

pub(crate) fn extract_app_icon(aumid: &str, target_size: usize) -> Option<Vec<u8>> {
    let size = target_size.clamp(8, 256);

    if let Some(pixels) = extract_from_aumid(aumid, size) {
        return Some(pixels);
    }

    if aumid.contains('\\') || aumid.contains("/.") {
        try_parsing_name(aumid, size)
    } else {
        None
    }
}
