//! Windows toast notifications, for problems that need attention while the window is hidden.

use crate::config::APP_IDENTIFIER;

/// Show a toast with a title and one line of text. It appears without taking focus and stays
/// in the notification centre; the installers register `APP_IDENTIFIER` so it carries the app's
/// name and icon. Failing to notify is logged and otherwise ignored.
pub fn toast(title: &str, body: &str) {
    #[cfg(windows)]
    if let Err(err) = show(title, body) {
        tracing::warn!("could not show a notification: {err}");
    }
    #[cfg(not(windows))]
    let _ = (title, body);
}

/// AppUserModelID passed to Windows toast notifications, matching the installer registry key.
pub const fn notification_app_id() -> &'static str {
    APP_IDENTIFIER
}

#[cfg(windows)]
fn show(title: &str, body: &str) -> windows::core::Result<()> {
    use windows::Data::Xml::Dom::XmlDocument;
    use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};
    use windows::core::HSTRING;

    let xml = XmlDocument::new()?;
    xml.LoadXml(&HSTRING::from(toast_xml(title, body)))?;
    let toast = ToastNotification::CreateToastNotification(&xml)?;
    ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(notification_app_id()))?
        .Show(&toast)
}

fn toast_xml(title: &str, body: &str) -> String {
    format!(
        "<toast><visual><binding template='ToastGeneric'><text>{}</text><text>{}</text></binding></visual></toast>",
        xml_text(title),
        xml_text(body)
    )
}

fn xml_text(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shows a real notification, so it only runs by hand:
    /// `cargo test -- --ignored show_puts_a_notification_on_screen`.
    #[cfg(windows)]
    #[test]
    #[ignore = "shows a real Windows notification"]
    fn show_puts_a_notification_on_screen() {
        show("Restic Tray", "通知の表示テストです").expect("toast shown");
    }

    #[test]
    fn toast_xml_escapes_markup_in_the_text() {
        assert_eq!(
            toast_xml("A & B", "<restic> failed"),
            "<toast><visual><binding template='ToastGeneric'><text>A &amp; B</text><text>&lt;restic&gt; failed</text></binding></visual></toast>"
        );
    }

    #[test]
    fn notification_app_id_matches_single_source_of_truth() {
        assert_eq!(notification_app_id(), APP_IDENTIFIER);
        assert_eq!(notification_app_id(), "io.github.nayuengin.restic-tray");
        assert_eq!(
            format!(r"Software\Classes\AppUserModelId\{}", notification_app_id()),
            r"Software\Classes\AppUserModelId\io.github.nayuengin.restic-tray"
        );
    }
}
