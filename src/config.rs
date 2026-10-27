//! 用户关机策略。只在用户保存修改时创建配置文件，启动读取失败不写盘。
use std::{
    io,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settings {
    pub heartbeat_timeout_seconds: u32,
    pub countdown_seconds: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            heartbeat_timeout_seconds: 60,
            countdown_seconds: 60,
        }
    }
}

/// 输入和文件共用严格校验，拒绝符号、小数、空白、零及溢出。
pub fn positive_seconds(value: &str) -> Option<u32> {
    if value.is_empty() || !value.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    value.parse::<u32>().ok().filter(|n| *n > 0)
}

impl Settings {
    pub fn path() -> Option<PathBuf> {
        std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .filter(|home| !home.is_empty())
            .map(|home| {
                PathBuf::from(home)
                    .join(".auto-shutdown")
                    .join("config.ini")
            })
    }

    pub fn load() -> Self {
        Self::path()
            .map(|path| Self::load_from(&path))
            .unwrap_or_default()
    }

    fn load_from(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| Self::parse(&text))
            .unwrap_or_default()
    }

    fn parse(text: &str) -> Option<Self> {
        let mut timeout = None;
        let mut countdown = None;
        for line in text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#') && !line.starts_with(';'))
        {
            let (key, value) = line.split_once('=')?;
            let slot = match key.trim() {
                "heartbeat_timeout_seconds" => &mut timeout,
                "countdown_seconds" => &mut countdown,
                _ => return None,
            };
            if slot.is_some() {
                return None;
            }
            *slot = Some(positive_seconds(value.trim())?);
        }
        Some(Self {
            heartbeat_timeout_seconds: timeout?,
            countdown_seconds: countdown?,
        })
    }

    pub fn save(&self) -> io::Result<()> {
        let path = Self::path()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "无法确定用户目录"))?;
        self.save_to(&path)
    }

    fn save_to(&self, path: &Path) -> io::Result<()> {
        use std::io::Write;
        if self.heartbeat_timeout_seconds == 0 || self.countdown_seconds == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "秒数必须大于 0",
            ));
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let temporary = path.with_extension(format!("ini.{}.tmp", std::process::id()));
        let result = (|| {
            let mut file = std::fs::File::create(&temporary)?;
            writeln!(
                file,
                "heartbeat_timeout_seconds={}\ncountdown_seconds={}",
                self.heartbeat_timeout_seconds, self.countdown_seconds
            )?;
            file.sync_all()?;
            drop(file);
            std::fs::rename(&temporary, path)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seconds_must_be_positive_decimal_integers() {
        for value in [
            "",
            "0",
            "00",
            "-1",
            "+1",
            "1.5",
            " 1",
            "1 ",
            "１",
            "4294967296",
        ] {
            assert_eq!(positive_seconds(value), None, "{value:?}");
        }
        assert_eq!(positive_seconds("1"), Some(1));
        assert_eq!(positive_seconds("4294967295"), Some(u32::MAX));
    }

    #[test]
    fn invalid_or_incomplete_file_uses_defaults() {
        for text in [
            "",
            "heartbeat_timeout_seconds=5",
            "heartbeat_timeout_seconds=0\ncountdown_seconds=60",
            "heartbeat_timeout_seconds=5\ncountdown_seconds=no",
            "heartbeat_timeout_seconds=5\ncountdown_seconds=6\ncountdown_seconds=7",
        ] {
            assert!(Settings::parse(text).is_none(), "{text:?}");
        }
    }

    #[test]
    fn reading_does_not_create_file_and_saving_round_trips_and_replaces() {
        let root = std::env::temp_dir().join(format!(
            "auto-shutdown-config-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = root.join("config.ini");
        assert_eq!(Settings::load_from(&path), Settings::default());
        assert!(!root.exists());
        let settings = Settings {
            heartbeat_timeout_seconds: 12,
            countdown_seconds: 34,
        };
        settings.save_to(&path).unwrap();
        assert_eq!(Settings::load_from(&path), settings);
        Settings::default().save_to(&path).unwrap();
        assert_eq!(Settings::load_from(&path), Settings::default());
        std::fs::write(&path, "broken").unwrap();
        assert_eq!(Settings::load_from(&path), Settings::default());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "broken");
        std::fs::remove_dir_all(root).unwrap();
    }
}
