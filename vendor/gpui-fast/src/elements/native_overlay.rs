use crate::{
    AnyElement, App, Bounds, Element, GlobalElementId, HitboxBehavior, InspectorElementId,
    IntoElement, LayoutId, NativeOverlayInput, Pixels, Point, Window,
};

/// Presents an existing subtree above native child windows without changing its
/// layout, view state, focus handles, or event dispatch tree.
pub fn native_overlay(child: impl IntoElement, input: NativeOverlayInput) -> NativeOverlay {
    NativeOverlay {
        child: child.into_any_element(),
        input,
    }
}

/// A presentation scope for native child-window overlays.
pub struct NativeOverlay {
    child: AnyElement,
    input: NativeOverlayInput,
}

impl IntoElement for NativeOverlay {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl Element for NativeOverlay {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<crate::ElementId> {
        None
    }
    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (self.child.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> () {
        window.with_native_overlay(self.input, |window| {
            if self.input == NativeOverlayInput::Modal {
                window.insert_hitbox(
                    Bounds::new(Point::default(), window.viewport_size()),
                    HitboxBehavior::BlockMouse,
                );
            }
            self.child.prepaint(window, cx);
        });
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        window.with_native_overlay(self.input, |window| self.child.paint(window, cx));
    }
}
