use super::super::super::list::{editor_stepper_field, required};
use super::{ConnectionEditorSectionContext, editor_field};
use crate::features::NyaTermApp;
use crate::models::{ConnectionEditorField, ConnectionEditorSection};
use gpui::{Context, div, prelude::*, px};
use nyaterm_ui::NyaSwitch;
use rust_i18n::t;

pub(super) fn section(
    section: ConnectionEditorSectionContext<'_>,
    cx: &mut Context<NyaTermApp>,
) -> gpui::Div {
    let ConnectionEditorSectionContext {
        palette,
        editor,
        fields,
        narrow,
        ..
    } = section;
    if editor.section != ConnectionEditorSection::Basic {
        return div();
    }
    let editor_usb = editor.harmony_usb;
    div()
        .flex()
        .flex_col()
        .gap_3()
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(t!("harmony.usbDevice"))
                .child(
                    NyaSwitch::new("harmony-usb")
                        .checked(editor_usb)
                        .on_click(cx.listener(move |app, _, _, cx| {
                            app.connection_state.set_harmony_editor_usb(!editor_usb);
                            app.connection_state.sync_editor_fields_from_draft(cx);
                            cx.notify();
                        })),
                ),
        )
        .child(
            div()
                .flex()
                .when(narrow, |this| this.flex_col())
                .gap_3()
                .child(div().min_w_0().flex_1().child(editor_field(
                    palette,
                    required(if editor_usb {
                        t!("harmony.connectKey")
                    } else {
                        t!("dialog.host")
                    }),
                    ConnectionEditorField::Host,
                    fields,
                    cx,
                )))
                .when(!editor_usb, |this| {
                    this.child(
                        div()
                            .when(!narrow, |this| this.w(px(150.)).flex_none())
                            .when(narrow, |this| this.w_full())
                            .child(editor_stepper_field(
                                palette,
                                required(t!("dialog.port")),
                                ConnectionEditorField::Port,
                                fields,
                                cx,
                            )),
                    )
                }),
        )
}
