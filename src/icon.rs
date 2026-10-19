/// llama.app 图标资源（由 `assets/llama-logo.svg` 栅格化生成）。
///
/// 同一个图标用于窗口/任务栏、系统托盘和界面标题；exe 文件图标由
/// `build.rs` 通过 Windows 资源嵌入 `assets/llama.ico`。
pub const LOGO_32_SIZE: u32 = 32;
/// 托盘图标（32×32 RGBA 原始像素）。
pub const LOGO_32_RGBA: &[u8] = include_bytes!("../assets/logo_32.rgba");

/// 窗口图标边长（像素）。
pub const LOGO_128_SIZE: u32 = 128;
/// 窗口 / 任务栏图标（128×128 RGBA 原始像素）。
pub const LOGO_128_RGBA: &[u8] = include_bytes!("../assets/logo_128.rgba");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logo_assets_have_expected_size() {
        assert_eq!(
            LOGO_32_RGBA.len(),
            (LOGO_32_SIZE * LOGO_32_SIZE * 4) as usize
        );
        assert_eq!(
            LOGO_128_RGBA.len(),
            (LOGO_128_SIZE * LOGO_128_SIZE * 4) as usize
        );
    }
}
