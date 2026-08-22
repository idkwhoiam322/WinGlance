//! UI Automation fragment provider that exposes the owner-drawn Settings pane to screen readers.
//!
//! The settings controls are owner-drawn, so Narrator/UIA see nothing without a
//! provider. One fragment-root provider is built on demand from the live
//! settings layout; each keyboard-focusable control becomes a child element
//! with a name, control type, enabled/focusable state, bounding rectangle, and
//! an Invoke (or Toggle) pattern. Activating a control posts its stable runtime
//! id to the main window, which re-resolves it against the live layout and
//! dispatches through the same function as a real mouse click, so no behavior is
//! duplicated.
//!
//! windows COM class conventions used here (windows-rs 0.62, the crate line
//! this module compiles against): the `#[implement]` macro generates a
//! `SettingsProvider_Impl` COM class whose `this` field holds our struct, and
//! the `*_Impl` traits are implemented for that generated type. "No value"
//! answers use `Err(Error::empty())` — the encoding the crate itself produces
//! for a null interface on the client side — and nullable SAFEARRAY out-params
//! use `Ok(null_mut())`.
//!
//! The provider only ever reads window state through the null-safe helpers in
//! `main_window`, so a provider instance that outlives window teardown (UIA
//! core holds a reference across the last release) degrades to empty answers
//! instead of reading freed memory.

use crate::winapi::post_message;
use std::sync::{Arc, Mutex};
use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{ClientToScreen, ScreenToClient};
use windows::Win32::System::Com::SAFEARRAY;
use windows::Win32::System::Ole::{SafeArrayCreateVector, SafeArrayDestroy, SafeArrayPutElement};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::System::Variant::VT_I4;
use windows::Win32::UI::Accessibility::{
    IInvokeProvider, IInvokeProvider_Impl, IRawElementProviderFragment, IRawElementProviderFragment_Impl,
    IRawElementProviderFragmentRoot, IRawElementProviderFragmentRoot_Impl, IRawElementProviderSimple,
    IRawElementProviderSimple_Impl, IToggleProvider, IToggleProvider_Impl, NavigateDirection,
    NavigateDirection_FirstChild, NavigateDirection_LastChild, NavigateDirection_NextSibling, NavigateDirection_Parent,
    NavigateDirection_PreviousSibling, ProviderOptions_ServerSideProvider, StructureChangeType_ChildrenInvalidated,
    ToggleState_Off, ToggleState_On, UIA_GroupControlTypeId, UIA_HasKeyboardFocusPropertyId, UIA_InvokePatternId,
    UIA_IsEnabledPropertyId, UIA_IsKeyboardFocusablePropertyId, UIA_NamePropertyId, UIA_PATTERN_ID, UIA_PROPERTY_ID,
    UIA_PaneControlTypeId, UIA_TextControlTypeId, UIA_TogglePatternId, UiaAppendRuntimeId, UiaHostProviderFromHwnd,
    UiaRaiseAutomationPropertyChangedEvent, UiaRaiseStructureChangedEvent, UiaRect, UiaReturnRawElementProvider,
};
use windows::core::implement;
use windows::core::{BSTR, Error, IUnknown, Interface};

/// One keyboard-focusable Settings control, as seen by UI Automation.
#[derive(Clone)]
pub struct SettingChild {
    pub row_index: usize,
    pub sub: crate::main_window::SettingSub,
    /// Bounding rectangle in client coordinates.
    pub rect: RECT,
    pub name: String,
    pub control_type: windows::Win32::UI::Accessibility::UIA_CONTROLTYPE_ID,
    /// Some(state) when the control is a toggle; None for plain buttons.
    pub toggle: Option<bool>,
    /// Stable per-control id used in the UIA runtime id. Derived from
    /// (row index, sub-control), so re-enumerating yields the same id and UIA
    /// clients keep their notion of the element across provider rebuilds.
    pub runtime_id: i32,
}

/// Which element a `SettingsProvider` answers for: the fragment root, or one
/// child identified by its STABLE (row, sub) identity. Identity — not an
/// index into an owned snapshot — is what lets a provider retained by UIA
/// core across a toggle, scroll, resize, or DPI change resolve the control's
/// CURRENT name, toggle state, and bounds on every query, and answer
/// unavailable after teardown.
#[derive(Clone)]
enum ProviderKind {
    Root,
    Child {
        row_index: usize,
        sub: crate::main_window::SettingSub,
    },
}

#[implement(
    IRawElementProviderSimple,
    IRawElementProviderFragment,
    IRawElementProviderFragmentRoot,
    IInvokeProvider,
    IToggleProvider
)]
struct SettingsProvider {
    hwnd: HWND,
    kind: ProviderKind,
}

impl SettingsProvider {
    /// Resolves this provider's control against the CURRENT Settings
    /// snapshot (generation-tagged, rebuilt by the UI thread on request), so
    /// every query answers from the latest painted state — a retained
    /// provider can never report a stale name, toggle state, focus, or
    /// rectangle. After window teardown the snapshot is cleared (WM_NCDESTROY
    /// empties it), resolution fails, and the provider degrades to
    /// unavailable answers instead of stale window data.
    fn resolve(&self) -> Option<SettingChild> {
        let ProviderKind::Child { row_index, sub } = &self.kind else {
            return None;
        };
        crate::main_window::settings_accessibility_children(self.hwnd)
            .into_iter()
            .find(|c| c.row_index == *row_index && c.sub == *sub)
    }

    fn make(&self, kind: ProviderKind) -> SettingsProvider {
        SettingsProvider { hwnd: self.hwnd, kind }
    }

    fn child_fragment(&self, row_index: usize, sub: crate::main_window::SettingSub) -> IRawElementProviderFragment {
        self.make(ProviderKind::Child { row_index, sub }).into()
    }

    fn root_fragment(&self) -> IRawElementProviderFragment {
        self.make(ProviderKind::Root).into()
    }

    fn root_fragment_root(&self) -> IRawElementProviderFragmentRoot {
        self.make(ProviderKind::Root).into()
    }

    fn screen_rect(&self, client: RECT) -> UiaRect {
        if self.hwnd.0.is_null() {
            return UiaRect {
                left: client.left as f64,
                top: client.top as f64,
                width: (client.right - client.left) as f64,
                height: (client.bottom - client.top) as f64,
            };
        }
        let mut p = POINT {
            x: client.left,
            y: client.top,
        };
        unsafe {
            let _ = ClientToScreen(self.hwnd, &mut p);
        }
        UiaRect {
            left: p.x as f64,
            top: p.y as f64,
            width: (client.right - client.left) as f64,
            height: (client.bottom - client.top) as f64,
        }
    }

    fn control_type(&self) -> windows::Win32::UI::Accessibility::UIA_CONTROLTYPE_ID {
        match &self.kind {
            ProviderKind::Root => UIA_PaneControlTypeId,
            ProviderKind::Child { .. } => self.resolve().map(|c| c.control_type).unwrap_or(UIA_GroupControlTypeId),
        }
    }

    fn name(&self) -> String {
        match &self.kind {
            ProviderKind::Root => "Settings".to_string(),
            ProviderKind::Child { .. } => self.resolve().map(|c| c.name).unwrap_or_default(),
        }
    }

    fn has_keyboard_focus(&self) -> bool {
        let ProviderKind::Child { .. } = &self.kind else {
            return false;
        };
        let Some(child) = self.resolve() else {
            return false;
        };
        crate::main_window::settings_focus(self.hwnd).is_some_and(|(r, s)| child.row_index == r && child.sub == s)
    }

    /// Activates the control by posting its stable runtime id to the main
    /// window, which re-resolves it against the live layout and dispatches
    /// through the same function as a real mouse click. The id is resolved
    /// through the CURRENT snapshot, so a provider held past teardown or a
    /// layout change cannot activate a control that no longer exists: a stale
    /// id finds no row and is dropped.
    fn activate(&self) {
        let ProviderKind::Child { .. } = &self.kind else {
            return;
        };
        let Some(child) = self.resolve() else {
            return;
        };
        if self.hwnd.0.is_null() {
            return;
        }
        let _ = unsafe {
            post_message(
                self.hwnd,
                crate::main_window::WM_SETTINGS_ACTIVATE_MSG,
                windows::Win32::Foundation::WPARAM(child.runtime_id as usize),
                windows::Win32::Foundation::LPARAM(0),
            )
        };
    }
}

impl IRawElementProviderSimple_Impl for SettingsProvider_Impl {
    fn ProviderOptions(&self) -> windows::core::Result<windows::Win32::UI::Accessibility::ProviderOptions> {
        Ok(ProviderOptions_ServerSideProvider)
    }

    fn GetPatternProvider(&self, patternid: UIA_PATTERN_ID) -> windows::core::Result<IUnknown> {
        let this = &self.this;
        let ProviderKind::Child { row_index, sub } = &this.kind else {
            return Err(Error::empty());
        };
        let Some(child) = this.resolve() else {
            return Err(Error::empty());
        };
        if child.toggle.is_some() && patternid == UIA_TogglePatternId {
            let p: IToggleProvider = this
                .make(ProviderKind::Child {
                    row_index: *row_index,
                    sub: *sub,
                })
                .into();
            return p.cast::<IUnknown>();
        }
        if child.toggle.is_none() && patternid == UIA_InvokePatternId {
            let p: IInvokeProvider = this
                .make(ProviderKind::Child {
                    row_index: *row_index,
                    sub: *sub,
                })
                .into();
            return p.cast::<IUnknown>();
        }
        Err(Error::empty())
    }

    fn GetPropertyValue(&self, propertyid: UIA_PROPERTY_ID) -> windows::core::Result<VARIANT> {
        let this = &self.this;
        if propertyid == UIA_NamePropertyId {
            return Ok(VARIANT::from(BSTR::from(this.name())));
        }
        if propertyid == windows::Win32::UI::Accessibility::UIA_ControlTypePropertyId {
            return Ok(VARIANT::from(this.control_type().0));
        }
        if propertyid == UIA_IsEnabledPropertyId {
            // A child that no longer resolves against the live snapshot is
            // gone: reporting it enabled would let a client activate or
            // target a control that no longer exists. The root stays
            // enabled — it is the pane surface itself.
            let enabled = match &this.kind {
                ProviderKind::Root => true,
                ProviderKind::Child { .. } => this.resolve().is_some(),
            };
            return Ok(VARIANT::from(enabled));
        }
        if propertyid == UIA_IsKeyboardFocusablePropertyId {
            let focusable = matches!(&this.kind, ProviderKind::Child { .. }) && this.resolve().is_some();
            return Ok(VARIANT::from(focusable));
        }
        if propertyid == UIA_HasKeyboardFocusPropertyId {
            return Ok(VARIANT::from(this.has_keyboard_focus()));
        }
        // BoundingRectangle is answered through IRawElementProviderFragment.
        Ok(VARIANT::default())
    }

    fn HostRawElementProvider(&self) -> windows::core::Result<IRawElementProviderSimple> {
        // The fragment root attaches to the HWND's default provider, which
        // carries the window-level semantics (title, window control type).
        // Child fragments are never queried for a host.
        let this = &self.this;
        if matches!(this.kind, ProviderKind::Root) && !this.hwnd.0.is_null() {
            unsafe { UiaHostProviderFromHwnd(this.hwnd) }
        } else {
            Err(Error::empty())
        }
    }
}

impl IRawElementProviderFragment_Impl for SettingsProvider_Impl {
    fn Navigate(&self, direction: NavigateDirection) -> windows::core::Result<IRawElementProviderFragment> {
        let this = &self.this;
        match &this.kind {
            ProviderKind::Root => {
                let children = crate::main_window::settings_accessibility_children(this.hwnd);
                if direction == NavigateDirection_FirstChild
                    && let Some(first) = children.first()
                {
                    return Ok(this.child_fragment(first.row_index, first.sub));
                }
                if direction == NavigateDirection_LastChild
                    && let Some(last) = children.last()
                {
                    return Ok(this.child_fragment(last.row_index, last.sub));
                }
            }
            ProviderKind::Child { row_index, sub } => {
                if direction == NavigateDirection_Parent {
                    return Ok(this.root_fragment());
                }
                // Sibling order is resolved against the CURRENT snapshot, so
                // after a scroll or layout change a retained provider
                // navigates the live tree, not the enumeration it was built
                // from.
                let children = crate::main_window::settings_accessibility_children(this.hwnd);
                let index = children.iter().position(|c| c.row_index == *row_index && c.sub == *sub);
                if direction == NavigateDirection_NextSibling
                    && let Some(index) = index
                    && let Some(next) = children.get(index + 1)
                {
                    return Ok(this.child_fragment(next.row_index, next.sub));
                }
                if direction == NavigateDirection_PreviousSibling
                    && let Some(index) = index
                    && index > 0
                    && let Some(prev) = children.get(index - 1)
                {
                    return Ok(this.child_fragment(prev.row_index, prev.sub));
                }
            }
        }
        Err(Error::empty())
    }

    fn GetRuntimeId(&self) -> windows::core::Result<*mut SAFEARRAY> {
        match &self.this.kind {
            // For the root, UIA derives the runtime id from the HWND; a null
            // array is the documented "no custom id" answer.
            ProviderKind::Root => Ok(std::ptr::null_mut()),
            ProviderKind::Child { row_index, sub } => {
                runtime_id_array(crate::main_window::setting_runtime_id(*row_index, *sub))
            }
        }
    }

    fn BoundingRectangle(&self) -> windows::core::Result<UiaRect> {
        let this = &self.this;
        let client = match &this.kind {
            ProviderKind::Root => Some(crate::main_window::settings_content_rect(this.hwnd)),
            ProviderKind::Child { .. } => this.resolve().map(|c| c.rect),
        };
        Ok(client.map_or(UiaRect::default(), |client| this.screen_rect(client)))
    }

    fn GetEmbeddedFragmentRoots(&self) -> windows::core::Result<*mut SAFEARRAY> {
        // No embedded roots: a null array means "none".
        Ok(std::ptr::null_mut())
    }

    fn SetFocus(&self) -> windows::core::Result<()> {
        if let ProviderKind::Child { row_index, sub } = &self.this.kind {
            // `focus_setting_at` validates the pair against the live layout
            // before committing, so a stale provider cannot focus a control
            // that no longer exists.
            crate::main_window::focus_setting_at(self.this.hwnd, *row_index, *sub);
        }
        Ok(())
    }

    fn FragmentRoot(&self) -> windows::core::Result<IRawElementProviderFragmentRoot> {
        Ok(self.this.root_fragment_root())
    }
}

impl IRawElementProviderFragmentRoot_Impl for SettingsProvider_Impl {
    fn ElementProviderFromPoint(&self, x: f64, y: f64) -> windows::core::Result<IRawElementProviderFragment> {
        let this = &self.this;
        if this.hwnd.0.is_null() {
            return Ok(this.root_fragment());
        }
        // Hit-test against the live layout with the exact control rectangles:
        // scrolling may have moved every control since enumeration, and the
        // bounds now equal the clickable targets.
        let children = crate::main_window::settings_accessibility_children(this.hwnd);
        let mut p = POINT {
            x: x as i32,
            y: y as i32,
        };
        if !unsafe { ScreenToClient(this.hwnd, &mut p) }.as_bool() {
            return Ok(this.root_fragment());
        }
        for child in &children {
            if p.x >= child.rect.left && p.x < child.rect.right && p.y >= child.rect.top && p.y < child.rect.bottom {
                return Ok(SettingsProvider {
                    hwnd: this.hwnd,
                    kind: ProviderKind::Child {
                        row_index: child.row_index,
                        sub: child.sub,
                    },
                }
                .into());
            }
        }
        Ok(this.root_fragment())
    }

    fn GetFocus(&self) -> windows::core::Result<IRawElementProviderFragment> {
        let Some((row, sub)) = crate::main_window::settings_focus(self.this.hwnd) else {
            return Err(Error::empty());
        };
        let children = crate::main_window::settings_accessibility_children(self.this.hwnd);
        if !children.iter().any(|c| c.row_index == row && c.sub == sub) {
            return Err(Error::empty());
        }
        Ok(SettingsProvider {
            hwnd: self.this.hwnd,
            kind: ProviderKind::Child { row_index: row, sub },
        }
        .into())
    }
}

impl IInvokeProvider_Impl for SettingsProvider_Impl {
    fn Invoke(&self) -> windows::core::Result<()> {
        self.this.activate();
        Ok(())
    }
}

impl IToggleProvider_Impl for SettingsProvider_Impl {
    fn Toggle(&self) -> windows::core::Result<()> {
        self.this.activate();
        Ok(())
    }

    fn ToggleState(&self) -> windows::core::Result<windows::Win32::UI::Accessibility::ToggleState> {
        // Resolved through the CURRENT snapshot: after the toggle flipped, a
        // retained provider reports the new state; after teardown it reports
        // the unavailable `Off` rather than a stale value.
        if let Some(on) = self.this.resolve().and_then(|c| c.toggle) {
            return Ok(if on { ToggleState_On } else { ToggleState_Off });
        }
        Ok(ToggleState_Off)
    }
}

/// Builds the two-element `[UiaAppendRuntimeId, id]` i32 SAFEARRAY UIA expects
/// from fragment children. Ownership transfers to the caller (UIA core).
fn runtime_id_array(id: i32) -> windows::core::Result<*mut SAFEARRAY> {
    let elements = [UiaAppendRuntimeId as i32, id];
    let array = unsafe { SafeArrayCreateVector(VT_I4, 0, elements.len() as u32) };
    if array.is_null() {
        return Err(Error::from_thread());
    }
    for (i, value) in elements.iter().enumerate() {
        if let Err(error) = unsafe { SafeArrayPutElement(array, &(i as i32), value as *const i32 as *const _) } {
            let _ = unsafe { SafeArrayDestroy(array) };
            return Err(error);
        }
    }
    Ok(array)
}

/// Read-only name provider for the passive overlay pill window. The pill is
/// deliberately non-focusable and non-clickable (it never takes keyboard
/// focus, never activates, and is click-through), so this provider exposes
/// exactly one thing: the current track as the accessible name. It offers no
/// patterns (no Invoke/Toggle — a screen reader cannot activate it), is never
/// keyboard-focusable, and only ever answers properties. The name itself is
/// read from a shared cell (`pill_name`) that the overlay UI thread updates
/// on every content change, so the provider never dereferences window state
/// off the UI thread — a provider instance that outlives the window (UIA
/// core holds a reference across the last release) degrades to an empty name
/// instead of reading freed memory.
#[implement(IRawElementProviderSimple)]
struct PillNameProvider {
    hwnd: HWND,
    name: Arc<Mutex<Option<String>>>,
}

impl IRawElementProviderSimple_Impl for PillNameProvider_Impl {
    fn ProviderOptions(&self) -> windows::core::Result<windows::Win32::UI::Accessibility::ProviderOptions> {
        Ok(ProviderOptions_ServerSideProvider)
    }

    fn GetPatternProvider(&self, _patternid: UIA_PATTERN_ID) -> windows::core::Result<IUnknown> {
        // No patterns at all: the pill is passive by architecture, so a
        // screen reader must never be able to activate, toggle, or select
        // anything on it. An empty error is the "no provider" answer.
        Err(Error::empty())
    }

    fn GetPropertyValue(&self, propertyid: UIA_PROPERTY_ID) -> windows::core::Result<VARIANT> {
        let this = &self.this;
        if propertyid == UIA_NamePropertyId {
            let name = this
                .name
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
                .unwrap_or_default();
            return Ok(VARIANT::from(BSTR::from(name)));
        }
        if propertyid == windows::Win32::UI::Accessibility::UIA_ControlTypePropertyId {
            return Ok(VARIANT::from(UIA_TextControlTypeId.0));
        }
        if propertyid == UIA_IsEnabledPropertyId {
            return Ok(VARIANT::from(true));
        }
        if propertyid == UIA_IsKeyboardFocusablePropertyId {
            // The pill never takes focus and never activates; a screen reader
            // must not be able to focus it.
            return Ok(VARIANT::from(false));
        }
        Ok(VARIANT::default())
    }

    fn HostRawElementProvider(&self) -> windows::core::Result<IRawElementProviderSimple> {
        // Merge with the window's own default provider (it carries the
        // window-level semantics — control type, window title) so the pill
        // is exposed as a readable element of the overlay window, not a
        // detached fragment.
        if self.this.hwnd.0.is_null() {
            return Err(Error::empty());
        }
        unsafe { UiaHostProviderFromHwnd(self.this.hwnd) }
    }
}

/// Builds the read-only name provider for the overlay pill window. `name` is
/// the shared cell the overlay updates on every content change. Used from the
/// overlay's `WM_GETOBJECT`.
pub fn pill_name_provider(hwnd: HWND, name: Arc<Mutex<Option<String>>>) -> IRawElementProviderSimple {
    PillNameProvider { hwnd, name }.into()
}

/// Raises the UIA name property-changed event for the pill, so a screen
/// reader tracking the pill announces the new track when the shared name
/// cell changes. Same fresh-provider-per-event pattern as the settings
/// pane's toggle/focus events; no UIA client listening is normal, so
/// failures log at debug level. Announcement only — it never makes the pill
/// focusable or activatable, preserving the passive architecture. The old
/// and new values are encoded exactly like `PillNameProvider` answers them
/// (an empty BSTR for a missing name), so the event matches what a client
/// would read from the property.
pub(crate) fn raise_pill_name_changed(
    hwnd: HWND,
    cell: &Arc<Mutex<Option<String>>>,
    old: Option<String>,
    new: Option<String>,
) {
    if hwnd.0.is_null() || old == new {
        return;
    }
    let provider = pill_name_provider(hwnd, Arc::clone(cell));
    let old = VARIANT::from(BSTR::from(old.unwrap_or_default()));
    let new = VARIANT::from(BSTR::from(new.unwrap_or_default()));
    if let Err(error) = unsafe { UiaRaiseAutomationPropertyChangedEvent(&provider, UIA_NamePropertyId, &old, &new) } {
        log::debug!("raising the pill name-changed UIA event failed: {error}");
    }
}

/// Builds the Settings-pane fragment-root provider, or None when there are no
/// focusable controls (e.g. the pane is hidden or the window is gone). Used
/// from `WM_GETOBJECT`.
pub fn settings_provider(hwnd: HWND) -> Option<IRawElementProviderSimple> {
    // The existence check gates provider attachment on the pane having
    // controls; the provider itself answers every query against fresh
    // snapshots, so it never serves a stale enumeration.
    if crate::main_window::settings_accessibility_children(hwnd).is_empty() {
        return None;
    }
    Some(
        SettingsProvider {
            hwnd,
            kind: ProviderKind::Root,
        }
        .into(),
    )
}

/// Raises one structure-invalidated event against the Settings fragment
/// root: a pane swap removes or adds the whole settings subtree, and
/// a client holding the old fragment otherwise never learns its answers
/// changed — the tree silently collapses or grows with no other signal.
/// `pre_captured` carries the root provider captured BEFORE a deactivating
/// flip (construction is gated on live children and would fail after them);
/// an activating flip passes `None` and the provider is built here, after
/// the children exist. Best-effort like every raise; failures log at debug.
pub(crate) fn raise_settings_structure_changed(hwnd: HWND, pre_captured: Option<IRawElementProviderSimple>) {
    let provider = pre_captured.or_else(|| settings_provider(hwnd));
    let Some(provider) = provider else {
        return;
    };
    unsafe {
        if let Err(error) = UiaRaiseStructureChangedEvent(
            &provider,
            StructureChangeType_ChildrenInvalidated,
            std::ptr::null_mut(),
            0,
        ) {
            log::debug!("raising the settings structure-changed UIA event failed: {error}");
        }
    }
}

/// Builds a provider for one Settings control identified by row and
/// sub-control, from the live layout. Used to raise UIA events (focus changed,
/// toggle state changed) on the element clients already know. The control's
/// existence is checked against the live layout (a vanished control gets no
/// event), and the returned provider resolves its properties against fresh
/// snapshots on every query, so an event raised on it always carries the
/// current state.
pub fn settings_child_provider(
    hwnd: HWND,
    row_index: usize,
    sub: crate::main_window::SettingSub,
) -> Option<IRawElementProviderSimple> {
    let children = crate::main_window::settings_accessibility_children(hwnd);
    if !children.iter().any(|c| c.row_index == row_index && c.sub == sub) {
        return None;
    }
    Some(
        SettingsProvider {
            hwnd,
            kind: ProviderKind::Child { row_index, sub },
        }
        .into(),
    )
}

/// Teardown contract for shared UIA provider state, called from every
/// window's WM_NCDESTROY. A provider handed to UIA core may outlive the
/// window (core holds a reference across the last release), so the window
/// clears the shared state it answered from: a client-held provider then
/// reads empty instead of window data. Both the main window's settings
/// snapshot and the overlay's pill name cell follow this; the lock is
/// recovered even when poisoned, so a panic while holding it can never keep
/// stale window data readable after teardown.
pub(crate) fn clear_uia_provider_state<T>(slot: &Mutex<Option<T>>) {
    *slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
}

/// Disconnects a window's UIA provider at WM_DESTROY, while the window and
/// its state still exist, so UIA core releases its references instead of
/// calling into a torn-down window later. Both the main window and the
/// overlay apply the same defensive detach.
pub(crate) fn detach_hwnd_provider(hwnd: HWND) {
    let _ = unsafe { UiaReturnRawElementProvider(hwnd, WPARAM(0), LPARAM(0), None) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_id_array_builds_uiaappend_plus_id() {
        // Read the produced SAFEARRAY back and check both elements. VT_I4 is
        // passed by value; SafeArrayGetElement copies into an i32.
        use windows::Win32::System::Ole::SafeArrayGetElement;
        let array = runtime_id_array(0x0503).expect("safearray allocation succeeds");
        assert!(!array.is_null());
        unsafe {
            let mut first = 0i32;
            let mut second = 0i32;
            let idx0 = 0i32;
            let idx1 = 1i32;
            SafeArrayGetElement(array, &idx0, &mut first as *mut i32 as *mut _)
                .ok()
                .unwrap();
            SafeArrayGetElement(array, &idx1, &mut second as *mut i32 as *mut _)
                .ok()
                .unwrap();
            assert_eq!(first, UiaAppendRuntimeId as i32);
            assert_eq!(second, 0x0503);
            SafeArrayDestroy(array).ok().unwrap();
        }
    }

    #[test]
    fn clear_uia_provider_state_empties_the_slot_and_recovers_poison() {
        // Plain clear: a slot that still holds data reads empty after the call.
        let slot: Mutex<Option<String>> = Mutex::new(Some("last track".into()));
        clear_uia_provider_state(&slot);
        assert!(slot.lock().unwrap().is_none());

        // Poisoned lock: a panic while holding the lock must not keep stale
        // window data readable after teardown — the recovery the overlay's
        // teardown previously lacked (it skipped the clear on poison).
        let slot: Mutex<Option<String>> = Mutex::new(Some("stale".into()));
        let _ = std::panic::catch_unwind(|| {
            let _guard = slot.lock().unwrap();
            panic!("simulated panic while holding the teardown lock");
        });
        assert!(slot.is_poisoned());
        clear_uia_provider_state(&slot);
        // The poison flag persists after recovery — every production reader
        // uses the same `unwrap_or_else(into_inner)` the helper does, so the
        // assertion mirrors that read pattern.
        let read = slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(read.is_none());
    }
}
