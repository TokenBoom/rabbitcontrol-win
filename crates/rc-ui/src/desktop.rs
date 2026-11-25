use rc_types::Resolution;

// 使用默认主显示器逻辑尺寸作为空闲画布初始尺寸。
pub fn initial_resolution() -> Resolution {
    Resolution::new(1280, 720)
}
