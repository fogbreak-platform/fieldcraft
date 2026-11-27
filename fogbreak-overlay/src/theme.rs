use eframe::egui;
use solidcraft_theme::{Custom, Tokens};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThemeMode {
    Dark,
    Light,
}

impl ThemeMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
        }
    }

    pub fn from_str_opt(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "light" | "day" => Some(Self::Light),
            "dark" | "night" => Some(Self::Dark),
            _ => None,
        }
    }

    pub fn is_dark(self) -> bool {
        matches!(self, Self::Dark)
    }
}

pub fn initial_theme_mode() -> ThemeMode {
    if let Some(win) = web_sys::window() {
        if let Ok(search) = win.location().search() {
            for part in search.trim_start_matches('?').split('&') {
                if let Some(val) = part.strip_prefix("theme=")
                    && let Some(mode) = ThemeMode::from_str_opt(val)
                {
                    return mode;
                }
            }
        }
        if let Ok(Some(storage)) = win.local_storage()
            && let Ok(Some(val)) = storage.get_item("fogbreak:theme")
            && let Some(mode) = ThemeMode::from_str_opt(&val)
        {
            return mode;
        }
    }
    ThemeMode::Dark
}

pub fn sync_dom_theme(mode: ThemeMode) {
    if let Some(win) = web_sys::window() {
        if let Some(doc) = win.document()
            && let Some(root) = doc.document_element()
        {
            let _ = root.set_attribute("data-theme", mode.as_str());
        }
        if let Ok(Some(storage)) = win.local_storage() {
            let _ = storage.set_item("fogbreak:theme", mode.as_str());
        }
    }
}

pub fn apply_theme(ctx: &egui::Context, mode: ThemeMode, app_dark: &mut bool) {
    let dark = mode.is_dark();
    *app_dark = dark;
    solidcraft_theme::set_dark(dark);
    fieldcraft_ui_egui::theme::apply(ctx, dark);

    let bg = if dark {
        egui::Color32::from_rgb(0x11, 0x18, 0x21)
    } else {
        egui::Color32::from_rgb(0xF7, 0xFA, 0xFC)
    };
    let panel = if dark {
        egui::Color32::from_rgb(0x16, 0x1F, 0x2B)
    } else {
        egui::Color32::from_rgb(0xF7, 0xFA, 0xFC)
    };
    let panel_header = if dark {
        egui::Color32::from_rgb(0x1D, 0x29, 0x38)
    } else {
        egui::Color32::from_rgb(0xED, 0xF3, 0xF7)
    };
    let row = if dark {
        egui::Color32::from_rgb(0x1C, 0x27, 0x35)
    } else {
        egui::Color32::from_rgb(0xE6, 0xEF, 0xF5)
    };
    let row_hover = if dark {
        egui::Color32::from_rgb(0x24, 0x34, 0x47)
    } else {
        egui::Color32::from_rgb(0xDC, 0xE8, 0xF1)
    };
    let border = if dark {
        egui::Color32::from_rgb(0x2F, 0x3F, 0x52)
    } else {
        egui::Color32::from_rgb(0xD1, 0xDB, 0xE4)
    };
    let ink = if dark {
        egui::Color32::from_rgb(0xF7, 0xFA, 0xFC)
    } else {
        egui::Color32::from_rgb(0x11, 0x18, 0x21)
    };
    let dim = if dark {
        egui::Color32::from_rgb(0x94, 0xA6, 0xB8)
    } else {
        egui::Color32::from_rgb(0x52, 0x64, 0x76)
    };
    let accent = egui::Color32::from_rgb(0x08, 0x7F, 0x9C);
    let accent_ui = if dark {
        egui::Color32::from_rgb(0x2B, 0xAD, 0xCD)
    } else {
        accent
    };
    let accent_soft = if dark {
        egui::Color32::from_rgb(0x12, 0x3A, 0x47)
    } else {
        egui::Color32::from_rgb(0xD3, 0xEC, 0xF2)
    };

    let mut tokens = Tokens::get();
    tokens.app_bar = if dark {
        egui::Color32::from_rgb(0x0D, 0x13, 0x1B)
    } else {
        panel_header
    };
    tokens.toolbar = panel;
    tokens.panel = panel;
    tokens.panel_header = panel_header;
    tokens.border = border;
    tokens.text = ink;
    tokens.text_dim = dim;
    tokens.accent = accent_ui;
    tokens.accent_soft = accent_soft;
    tokens.viewport_top = bg;
    tokens.viewport_bottom = panel_header;
    tokens.dialog_bg = panel;
    tokens.dialog_body = panel;
    tokens.dialog_footer = panel_header;
    tokens.dialog_border = border;
    tokens.field = bg;
    tokens.control_fill = row;
    tokens.control_border = border;
    tokens.on_accent = egui::Color32::WHITE;
    tokens.app_bar_text = ink;
    tokens.app_bar_hover = row_hover;
    tokens.app_bar_separator = border;
    tokens.file_background = bg;
    tokens.file_surface = panel;
    tokens.file_border = border;
    tokens.file_primary = accent;
    tokens.file_primary_ink = egui::Color32::WHITE;
    solidcraft_theme::set_custom(Some(Custom { dark, tokens }));
    solidcraft_theme::apply(ctx);

    ctx.all_styles_mut(|style| {
        let mut visuals = if dark { egui::Visuals::dark() } else { egui::Visuals::light() };
        visuals.dark_mode = dark;
        visuals.panel_fill = panel;
        visuals.window_fill = panel;
        visuals.extreme_bg_color = bg;
        visuals.text_edit_bg_color = Some(bg);
        visuals.faint_bg_color = row;
        visuals.code_bg_color = bg;
        visuals.override_text_color = Some(ink);
        visuals.selection.bg_fill = accent;
        visuals.selection.stroke = egui::Stroke::new(1.0, egui::Color32::WHITE);
        visuals.hyperlink_color = accent_ui;
        visuals.widgets.active.bg_fill = accent;
        visuals.widgets.hovered.weak_bg_fill = row_hover;
        visuals.widgets.inactive.weak_bg_fill = row;
        style.visuals = visuals;
    });
}
