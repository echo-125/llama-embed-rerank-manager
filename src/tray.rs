use tray_icon::{
    Icon, TrayIcon, TrayIconBuilder, TrayIconEvent,
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
};

use crate::icon;
use crate::lang::Language;

/// 系统托盘及其菜单项（菜单项句柄用于比对点击事件）。
pub struct SystemTray {
    tray_icon: TrayIcon,
    pub item_show: MenuItem,
    pub item_start_all: MenuItem,
    pub item_stop_all: MenuItem,
    pub item_exit: MenuItem,
}

impl SystemTray {
    /// 创建托盘图标与中英文菜单。
    pub fn new(lang: Language) -> Result<Self, String> {
        let icon = create_tray_icon()?;

        let menu = Menu::new();
        let item_show = MenuItem::new(lang.t("显示主界面", "Show window"), true, None);
        let item_start_all = MenuItem::new(lang.t("启动全部模型", "Start all models"), true, None);
        let item_stop_all = MenuItem::new(lang.t("停止全部模型", "Stop all models"), true, None);
        let item_exit = MenuItem::new(lang.t("退出程序", "Exit"), true, None);

        menu.append(&item_show).map_err(|e| e.to_string())?;
        menu.append(&PredefinedMenuItem::separator())
            .map_err(|e| e.to_string())?;
        menu.append(&item_start_all).map_err(|e| e.to_string())?;
        menu.append(&item_stop_all).map_err(|e| e.to_string())?;
        menu.append(&PredefinedMenuItem::separator())
            .map_err(|e| e.to_string())?;
        menu.append(&item_exit).map_err(|e| e.to_string())?;

        let tray_icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip(lang.window_title())
            .with_icon(icon)
            .build()
            .map_err(|e| format!("创建系统托盘失败: {e}"))?;

        Ok(Self {
            tray_icon,
            item_show,
            item_start_all,
            item_stop_all,
            item_exit,
        })
    }

    /// 切换托盘菜单语言。
    pub fn set_language(&self, lang: Language) {
        self.item_show.set_text(lang.t("显示主界面", "Show window"));
        self.item_start_all
            .set_text(lang.t("启动全部模型", "Start all models"));
        self.item_stop_all
            .set_text(lang.t("停止全部模型", "Stop all models"));
        self.item_exit.set_text(lang.t("退出程序", "Exit"));
        let _ = self.tray_icon.set_tooltip(Some(lang.window_title()));
    }
}

/// 非阻塞地取出一个托盘事件；UI 循环每帧调用。
pub fn check_tray_events() -> Option<TrayAction> {
    if let Ok(event) = TrayIconEvent::receiver().try_recv() {
        if let TrayIconEvent::Click { button, .. } = event {
            if button == tray_icon::MouseButton::Left {
                return Some(TrayAction::ToggleShow);
            }
        }
    }

    if let Ok(event) = MenuEvent::receiver().try_recv() {
        return Some(TrayAction::Menu(event.id));
    }

    None
}

#[derive(Debug, PartialEq, Eq)]
/// 托盘交互结果：切换窗口显示，或点了某个菜单项。
pub enum TrayAction {
    ToggleShow,
    Menu(tray_icon::menu::MenuId),
}

fn create_tray_icon() -> Result<Icon, String> {
    Icon::from_rgba(
        icon::LOGO_32_RGBA.to_vec(),
        icon::LOGO_32_SIZE,
        icon::LOGO_32_SIZE,
    )
    .map_err(|e| format!("生成托盘图标失败: {e}"))
}
