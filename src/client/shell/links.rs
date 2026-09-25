//! Web links a client opens: in this machine's browser, or, when the screen
//! showing Herdr is elsewhere (SSH, mosh), in a dialog to tap or copy.

use super::*;

/// A link shown instead of opened, because a browser here would open on the
/// wrong screen.
#[derive(Debug)]
pub(super) struct ClientLinkOverlay {
    pub(super) url: String,
}

impl ClientShellState {
    /// Opens a vetted web link where the person using this client can see it.
    pub(super) fn open_web_link(&mut self, url: String, outcome: &mut ClientShellInput) {
        if self.config.shows_links() {
            self.overlay = Some(ClientShellOverlay::Link(ClientLinkOverlay { url }));
            outcome.repaint = true;
        } else {
            outcome.actions.push(ClientShellAction::OpenSafeWebUrl(url));
        }
    }

    /// Handles keys while the link dialog is open. Returns false for other overlays.
    pub(super) fn route_link_overlay_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) -> bool {
        if !matches!(self.overlay, Some(ClientShellOverlay::Link(_))) {
            return false;
        }
        match key.code {
            KeyCode::Enter | KeyCode::Char('c' | 'y') => self.copy_link_overlay(outcome),
            KeyCode::Esc | KeyCode::Char('q') => self.close_link_overlay(outcome),
            _ => {}
        }
        true
    }

    /// Clicks on the link or the copy button copy; outside the dialog closes it.
    pub(super) fn handle_link_overlay_click(
        &mut self,
        point: (u16, u16),
        outcome: &mut ClientShellInput,
    ) {
        if super::contains(self.hits.overlay_primary, point)
            || super::contains(self.hits.overlay_clear, point)
        {
            self.copy_link_overlay(outcome);
        } else if super::contains(self.hits.overlay_cancel, point)
            || !super::contains(self.hits.overlay_area, point)
        {
            self.close_link_overlay(outcome);
        }
    }

    fn copy_link_overlay(&mut self, outcome: &mut ClientShellInput) {
        let Some(ClientShellOverlay::Link(link)) = self.overlay.take() else {
            return;
        };
        // Over SSH and mosh the clipboard write travels as OSC 52, so it lands
        // on the device showing Herdr.
        outcome
            .actions
            .push(ClientShellAction::ClipboardWrite(link.url.into_bytes()));
        self.show_copy_feedback(std::time::Instant::now());
        outcome.repaint = true;
    }

    fn close_link_overlay(&mut self, outcome: &mut ClientShellInput) {
        self.overlay = None;
        outcome.repaint = true;
    }
}
