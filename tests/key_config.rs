#[path = "../build/key_config.rs"]
mod key_config;

#[test]
fn accepts_bom_comments_crlf_and_whitespace() {
    let config = format!("\u{feff}# 本地构建配置\r\n AUTO_SHUTDOWN_RADIO_KEY = {} \r\n", "aB".repeat(32));
    assert_eq!(key_config::parse(&config).unwrap(), [0xab; 32]);
}

#[test]
fn rejects_missing_blank_and_duplicate_keys() {
    assert!(key_config::parse("").is_err());
    assert!(key_config::parse(include_str!("../.env.example")).is_err());
    let line = format!("AUTO_SHUTDOWN_RADIO_KEY={}\n", "11".repeat(32));
    assert!(key_config::parse(&(line.clone() + &line)).is_err());
}

#[test]
fn rejects_zero_malformed_quoted_and_wrong_length_keys() {
    for value in ["0".repeat(64), "g".repeat(64), "中".repeat(64), "a".repeat(63),
        "a".repeat(65), format!("\"{}\"", "a".repeat(64))] {
        assert!(key_config::parse(&format!("AUTO_SHUTDOWN_RADIO_KEY={value}")).is_err());
    }
}

#[test]
fn errors_never_echo_secret_values() {
    let secret = "private-value-must-not-appear";
    let error = key_config::parse(&format!("AUTO_SHUTDOWN_RADIO_KEY={secret}")).unwrap_err();
    assert!(!error.contains(secret));
}
