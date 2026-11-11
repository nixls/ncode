//! Assets: GPUI Kit's default component icons plus the few extra Lucide icons ncode uses.

use std::borrow::Cow;

use gpui_kit::{AssetSource, Result, SharedString};

pub use gpui_kit::assets::IconName as AppIcon;

gpui_kit::assets::icon_assets!(
    ExtraIcons,
    [
        GitBranch,
        GitPullRequest,
        FolderPlus,
        Trash,
        Pencil,
        SquarePen,
        Wrench,
        Brain,
        ShieldCheck,
        CircleStop,
        MessageSquare
    ]
);

pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some(bytes) = ExtraIcons.load(path)? {
            return Ok(Some(bytes));
        }
        gpui_kit::assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut paths = gpui_kit::assets::Assets.list(path)?;
        paths.extend(ExtraIcons.list(path)?);
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}
