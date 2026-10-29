//! Linux partition resizing is handed to `GParted`, never to shell snippets.

use disktree_core::partition_editor::{self, Target};
use gpui_kit::{
    Context, InteractiveElement as _, IntoElement, ParentElement, Styled, div,
};
use gpui_omarchy::{
    ButtonVariant, Status, alert_dialog, dialog_button, dialog_description,
    dialog_popup, dialog_title,
};

use crate::state::{Disktree, FocusTarget};
use crate::ui::{size, space};
use crate::views::centred_popup;

#[derive(Debug)]
pub enum Dialog {
    Loading,
    Ready(Target),
    Editing,
    Failed(String),
}

impl Disktree {
    pub fn open_resize(&mut self, cx: &mut Context<'_, Self>) {
        if !self.supports_resize() || !self.can_start_over() {
            return;
        }
        self.resize_epoch += 1;
        let epoch = self.resize_epoch;
        self.resize_dialog = Some(Dialog::Loading);
        self.focus_request = Some(FocusTarget::Dialog);
        let root = self.root_path.clone();
        let task = cx.background_executor().spawn(async move {
            partition_editor::prepare(&root).map_err(|error| error.to_string())
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.resize_epoch == epoch && this.resize_open() {
                    this.resize_dialog = Some(match result {
                        Ok(target) => Dialog::Ready(target),
                        Err(error) => Dialog::Failed(error),
                    });
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    pub fn close_resize(&mut self, cx: &mut Context<'_, Self>) {
        if matches!(self.resize_dialog, Some(Dialog::Editing)) {
            return;
        }
        self.resize_epoch += 1;
        self.resize_dialog = None;
        self.focus_request = Some(FocusTarget::Root);
        cx.notify();
    }

    pub fn launch_partition_editor(&mut self, cx: &mut Context<'_, Self>) {
        // Enter while discovery is pending or after launch cannot start an
        // editor. A local Ready target is the only actionable state.
        if self.remote.is_some() {
            return;
        }
        let Some(Dialog::Ready(target)) = &self.resize_dialog else {
            return;
        };
        let target = target.clone();
        self.resize_dialog = Some(Dialog::Editing);
        let task = cx.background_executor().spawn(async move {
            partition_editor::open(&target).map_err(|error| error.to_string())
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(()) => {
                        this.resize_dialog = None;
                        this.focus_request = Some(FocusTarget::Root);
                        this.notice = Some((
                            "Partition editor closed; refreshing disk usage."
                                .into(),
                            Status::Neutral,
                        ));
                        this.start_scan(cx);
                    }
                    Err(error) => {
                        this.resize_dialog = Some(Dialog::Failed(error));
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}

pub fn dialog(
    app: &Disktree,
    cx: &mut Context<'_, Disktree>,
) -> impl IntoElement {
    let ready = matches!(app.resize_dialog, Some(Dialog::Ready(_)));
    let editing = matches!(app.resize_dialog, Some(Dialog::Editing));
    let message = match &app.resize_dialog {
        Some(Dialog::Ready(target)) => format!(
            "Open {} in GParted to grow or shrink {}. \
             Select it there, choose Resize/Move, then review \
             and apply the queued changes.",
            target.disk.display(),
            target.partition.as_ref().map_or_else(
                || "a partition".into(),
                |partition| partition.display().to_string()
            )
        ),
        Some(Dialog::Loading) => {
            "Finding the disk and partition editor…".into()
        }
        Some(Dialog::Editing) => "GParted is open. Close it to return here; \
            disk usage will refresh afterward."
            .into(),
        Some(Dialog::Failed(error)) => error.clone(),
        None => String::new(),
    };
    let popup = dialog_popup(cx)
        .w(size::SOURCE_PICKER)
        .child(dialog_title("Resize partition", cx))
        .child(dialog_description(message, cx))
        .child(dialog_description(
            "Growing needs adjacent unallocated space. Shrinking depends on \
             the filesystem. Resizing does not change a physical disk's capacity.", cx))
        .child(dialog_description(
            "Back up important files first. Use a live USB if the system \
             partition is busy or the operation requires unmounting. \
             Encrypted volumes must be unlocked in GParted; support depends \
             on the storage layout. No changes are applied by opening the editor.", cx))
        .child(div().flex().flex_row().justify_end().gap(space::SM)
            .child(dialog_button("resize-cancel", "Close", ButtonVariant::Secondary, cx)
                .disabled(editing)
                .debug_selector(|| "resize-cancel".into())
                .on_click(cx.listener(|this, _, window, cx| {
                    this.close_resize(cx);
                    this.apply_focus(window, cx);
                })))
            .child(dialog_button("resize-open", "Open GParted", ButtonVariant::Primary, cx)
                .disabled(!ready)
                .debug_selector(|| "resize-open".into())
                .on_click(cx.listener(|this, _, _, cx| this.launch_partition_editor(cx)))));
    let cancel = cx.entity().downgrade();
    let outside = cancel.clone();
    let accept = cancel.clone();
    let popup = div().debug_selector(|| "resize-dialog".into()).child(popup);
    alert_dialog(&app.confirm_focus, cx)
        .open(true)
        .on_ok(move |_, _, cx| {
            let _ = accept.update(cx, |this, cx| {
                this.launch_partition_editor(cx);
            });
            false
        })
        .on_cancel(move |_, window, cx| {
            let _ = cancel.update(cx, |this, cx| {
                this.close_resize(cx);
                this.apply_focus(window, cx);
            });
            false
        })
        .popup(centred_popup(popup, move |_, window, cx| {
            let _ = outside.update(cx, |this, cx| {
                this.close_resize(cx);
                this.apply_focus(window, cx);
            });
        }))
}
