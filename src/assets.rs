//! 默认组件资源加上应用使用的额外图标。
use gpui_kit::{AssetSource, Result, SharedString};
use std::borrow::Cow;

gpui_kit::assets::icon_assets!(HelpAssets, [CircleQuestionMark]);

pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some(bytes) = HelpAssets.load(path)? {
            return Ok(Some(bytes));
        }
        gpui_kit::assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut paths = gpui_kit::assets::Assets.list(path)?;
        paths.extend(HelpAssets.list(path)?);
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_and_default_icons_are_embedded() {
        for icon in [
            gpui_kit::assets::IconName::CircleQuestionMark,
            gpui_kit::assets::IconName::Info,
            gpui_kit::assets::IconName::ChevronDown,
        ] {
            let path = icon.path();
            let bytes = AppAssets.load(&path).unwrap().expect("图标资源必须打包");
            assert!(std::str::from_utf8(&bytes).unwrap().contains("<svg"));
            assert!(AppAssets.list("icons/").unwrap().contains(&path));
        }
    }
}
