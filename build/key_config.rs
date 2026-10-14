/// 严格读取本地构建密钥；错误信息不得包含配置值。
pub fn parse(config: &str) -> Result<[u8; 32], &'static str> {
    let mut values = config.trim_start_matches('\u{feff}').lines().filter_map(|line| {
        let line = line.trim();
        if line.starts_with('#') {
            return None;
        }
        let (name, value) = line.split_once('=')?;
        (name.trim() == "AUTO_SHUTDOWN_RADIO_KEY").then_some(value.trim())
    });
    let value = values.next().ok_or(".env.local 缺少 AUTO_SHUTDOWN_RADIO_KEY")?;
    if values.next().is_some() {
        return Err(".env.local 的 AUTO_SHUTDOWN_RADIO_KEY 不能重复");
    }
    if value.len() != 64 || !value.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err("AUTO_SHUTDOWN_RADIO_KEY 必须是 64 位十六进制随机密钥（不带引号）");
    }
    let mut key = [0; 32];
    for (i, byte) in key.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[i * 2..i * 2 + 2], 16).unwrap();
    }
    if key.iter().all(|&b| b == 0) {
        return Err("不能使用全零示例密钥");
    }
    Ok(key)
}
