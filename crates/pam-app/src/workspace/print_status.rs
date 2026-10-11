//! Print status (to print / printed / failed) and the free-text note kept
//! per asset.

use std::time::Duration;

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::TextareaState;
use gpui_kit::component::{h_flex, Icon, Sizable as _};
use gpui_kit::{prelude::FluentBuilder as _, *};
use pam_core::catalog::{Asset, PrintStatus};

use crate::i18n::{self, Key};

use super::format::format_date;
use super::Workspace;

/// Pause in typing after which the note is saved.
const NOTE_SAVE_DELAY: Duration = Duration::from_millis(600);

pub(super) fn status_key(status: PrintStatus) -> Key {
    match status {
        PrintStatus::Todo => Key::StatusTodo,
        PrintStatus::Printed => Key::StatusPrinted,
        PrintStatus::Failed => Key::StatusFailed,
    }
}

pub(super) fn status_icon(status: PrintStatus) -> Icon {
    Icon::new(match status {
        PrintStatus::Todo => Lucide::Clock,
        PrintStatus::Printed => Lucide::CircleCheck,
        PrintStatus::Failed => Lucide::CircleX,
    })
}

pub(super) fn status_color(status: PrintStatus, theme: &gpui_kit::component::Theme) -> Hsla {
    match status {
        PrintStatus::Todo => theme.warning,
        PrintStatus::Printed => theme.success,
        PrintStatus::Failed => theme.danger,
    }
}

/// Small colored icon for cards and list rows; nothing when unmarked.
pub(super) fn status_badge(
    asset: &Asset,
    theme: &gpui_kit::component::Theme,
) -> Option<impl IntoElement> {
    let status = asset.status?;
    Some(
        div()
            .flex()
            .items_center()
            .justify_center()
            .text_color(status_color(status, theme))
            .child(status_icon(status).xsmall()),
    )
}

/// When the status was set, as "2026-10-10".
pub(super) fn status_date(asset: &Asset) -> Option<String> {
    asset.status?;
    Some(format_date(asset.status_at? * 1_000_000_000))
}

/// One toggle button per status for `ids`: clicking sets it, clicking the
/// one that is already on (`current`) clears it.
pub(super) fn status_buttons(
    id_prefix: &str,
    current: Option<PrintStatus>,
    ids: Vec<i64>,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    h_flex().gap_1().children(PrintStatus::ALL.map(|status| {
        let on = current == Some(status);
        let ids = ids.clone();
        Button::new(SharedString::from(format!(
            "{id_prefix}-{}",
            status.as_str()
        )))
        .small()
        // On is a neutral fill, not the accent: the accent is reserved for
        // the one primary action.
        .when(on, |b| b.secondary())
        .when(!on, |b| b.outline())
        .icon(status_icon(status))
        .label(i18n::t(status_key(status)))
        .on_click(
            cx.listener(move |this, _, _, cx| this.set_status(&ids, (!on).then_some(status), cx)),
        )
    }))
}

impl Workspace {
    pub(super) fn set_status(
        &mut self,
        ids: &[i64],
        status: Option<PrintStatus>,
        cx: &mut Context<Self>,
    ) {
        if let Err(err) = self.catalog.set_status(ids, status) {
            eprintln!("set print status failed: {err}");
        }
        self.reload(cx);
    }

    /// Show `asset`'s note in the inspector, saving the one being edited first.
    pub(super) fn load_note(
        &mut self,
        asset: Option<&Asset>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.save_note(cx);
        self.note_asset = asset.map(|a| (a.id, a.note.clone()));
        let note = asset.map(|a| a.note.clone()).unwrap_or_default();
        self.note_input
            .update(cx, |input, cx| input.set_value(note, window, cx));
    }

    /// The inspector's tag and note inputs show the primary selection, also
    /// after a reload picked a different one than the user clicked.
    pub(super) fn sync_inspector_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let asset = self.selected_asset().cloned();
        if self.note_asset.as_ref().map(|(id, _)| *id) == asset.as_ref().map(|a| a.id) {
            return;
        }
        let tags = asset
            .as_ref()
            .map(|a| a.tags.join(", "))
            .unwrap_or_default();
        self.tag_input
            .update(cx, |input, cx| input.set_value(tags, window, cx));
        self.load_note(asset.as_ref(), window, cx);
    }

    /// Save the note after typing pauses.
    pub(super) fn schedule_note_save(&mut self, cx: &mut Context<Self>) {
        self.note_generation += 1;
        let generation = self.note_generation;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(NOTE_SAVE_DELAY).await;
            this.update(cx, |this, cx| {
                if this.note_generation == generation {
                    this.save_note(cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Write the inspector's note to the asset it belongs to, if it changed.
    pub(super) fn save_note(&mut self, cx: &mut Context<Self>) {
        let Some((id, saved)) = &mut self.note_asset else {
            return;
        };
        let text = self.note_input.read(cx).value().to_string();
        if *saved == text {
            return;
        }
        if let Err(err) = self.catalog.set_note(*id, &text) {
            eprintln!("save note failed: {err}");
            return;
        }
        let id = *id;
        saved.clone_from(&text);
        if let Some(asset) = self.assets.iter_mut().find(|a| a.id == id) {
            asset.note = text;
        }
        // Search covers notes, so saved-search counts can change. The list
        // itself waits for the next reload: re-querying here could drop the
        // asset being typed into from under the cursor.
        self.reload_saved();
        cx.notify();
    }
}

/// The note editor, sized for a few lines.
pub(super) fn new_note_input(
    window: &mut Window,
    cx: &mut Context<TextareaState>,
) -> TextareaState {
    TextareaState::new(window, cx)
        .auto_grow(2, 6)
        .placeholder(i18n::t(Key::NoteHint))
}
