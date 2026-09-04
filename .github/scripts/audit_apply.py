from pathlib import Path
import subprocess


def replace(path: str, old: str, new: str) -> None:
    p = Path(path)
    text = p.read_text(encoding="utf-8")
    if old not in text:
        raise SystemExit(f"expected text not found in {path}: {old[:220]!r}")
    p.write_text(text.replace(old, new, 1), encoding="utf-8")


def insert_before(path: str, marker: str, addition: str) -> None:
    replace(path, marker, addition + marker)


def insert_before_final_brace(path: str, addition: str) -> None:
    p = Path(path)
    text = p.read_text(encoding="utf-8")
    pos = text.rfind("\n}")
    if pos < 0:
        raise SystemExit(f"final brace not found in {path}")
    p.write_text(text[:pos] + addition + text[pos:], encoding="utf-8")

# --- Settings focus scrolling + effective AA title color --------------------
p = "src/main_window.rs"
insert_before(
    p,
    "impl MainWindowState {\n",
    '''/// Computes the document scroll offset needed to recenter a Settings\n/// focus target whose `client_y` is already in client coordinates. Keeping\n/// the current scroll separate is the important invariant: adding it to the\n/// visibility comparison double-counts the offset once the pane has scrolled.\nfn settings_focus_scroll_y(current_scroll: i32, client_y: i32, client_h: i32, row_h: i32) -> i32 {\n    let margin = row_h / 2;\n    if client_y < margin || client_y > client_h - margin {\n        current_scroll.saturating_add(client_y - client_h / 2)\n    } else {\n        current_scroll\n    }\n}\n\n''',
)
replace(
    p,
    '''        if t.cy < self.settings_scroll_y + row_h / 2 || t.cy > self.settings_scroll_y + client_h - row_h / 2 {\n            self.settings_scroll_y = t.cy - client_h / 2;\n            self.sync_settings_scroll(client_w, client_h);\n        }\n''',
    '''        let new_scroll = settings_focus_scroll_y(self.settings_scroll_y, t.cy, client_h, row_h);\n        if new_scroll != self.settings_scroll_y {\n            self.settings_scroll_y = new_scroll;\n            self.sync_settings_scroll(client_w, client_h);\n        }\n''',
)
replace(
    p,
    '''        let accent_color = self.accent_color;\n        let text_color = self.cfg().appearance.text_color;\n''',
    '''        let accent_color = self.accent_color;\n        // Keep the user's serialized color untouched, but never render the\n        // large Now Playing title below WCAG AA contrast against this pane's\n        // actual black surface. `ensure_contrast` is shared with the overlay,\n        // so both UI surfaces use the same 4.5:1 floor.\n        let text_color = crate::overlay::ensure_contrast(\n            self.cfg().appearance.text_color,\n            [0, 0, 0, 0xFF],\n            crate::overlay::TEXT_CONTRAST_AA,\n        );\n''',
)
insert_before_final_brace(
    p,
    '''\n\n    #[test]\n    fn settings_focus_scroll_uses_client_coordinates_from_nonzero_offsets() {\n        let current = 300;\n        let client_h = 600;\n        let row_h = 34;\n\n        // A target already inside the viewport must not move merely because\n        // the document itself is scrolled. The old comparison added `current`\n        // to the client-space bounds and incorrectly moved this target.\n        assert_eq!(settings_focus_scroll_y(current, 280, client_h, row_h), current);\n\n        // Offscreen-near-top and offscreen-near-bottom targets recenter around\n        // their document position by applying a delta to the existing scroll.\n        assert_eq!(settings_focus_scroll_y(current, 8, client_h, row_h), 8);\n        assert_eq!(settings_focus_scroll_y(current, 592, client_h, row_h), 592);\n    }\n''',
)

# --- Picker UI Automation fragment -----------------------------------------
p = "src/process_picker.rs"
replace(
    p,
    "pub(crate) const PINNED_SOURCE_RESULT_MSG: u32 = WM_APP + 12;\n",
    '''pub(crate) const PINNED_SOURCE_RESULT_MSG: u32 = WM_APP + 12;\n/// Posted by the picker UIA provider to the picker window. `wParam` is the\n/// row index; `lParam == 0` toggles through the same path as mouse/Space and\n/// `lParam == 1` only moves list focus to the row.\npub(crate) const PICKER_UIA_ACTION_MSG: u32 = WM_APP + 15;\n''',
)
replace(
    p,
    "    WM_DPICHANGED, WM_DRAWITEM, WM_KEYDOWN, WM_LBUTTONDOWN, WM_MOUSEMOVE, WM_NCCREATE, WM_NCDESTROY, WM_PAINT,\n",
    "    WM_DPICHANGED, WM_DRAWITEM, WM_GETOBJECT, WM_KEYDOWN, WM_LBUTTONDOWN, WM_MOUSEMOVE, WM_NCCREATE, WM_NCDESTROY, WM_PAINT,\n",
)
insert_before(
    p,
    "fn post_result(hwnd: HWND, cancelled: bool) {\n",
    '''#[derive(Clone)]\npub(crate) struct PickerAccessibleRow {\n    pub index: usize,\n    pub name: String,\n    pub checked: bool,\n    pub toggleable: bool,\n    /// Listbox-client coordinates; UIA converts these to screen coordinates.\n    pub rect: RECT,\n}\n\n/// Live UIA snapshot for the owner-drawn picker. The checkbox state comes from\n/// the same `LB_GETITEMDATA` source of truth used by painting, mouse clicks,\n/// keyboard Space and result collection, so accessibility can never drift from\n/// what the user sees or what gets saved.\npub(crate) fn picker_accessibility_rows(parent: HWND) -> Vec<PickerAccessibleRow> {\n    let state_ptr = window_state::<PickerState>(parent);\n    if state_ptr.is_null() {\n        return Vec::new();\n    }\n    let state = unsafe { &*state_ptr };\n    if state.listbox.0.is_null() {\n        return Vec::new();\n    }\n    let fixed_status = state.list.first().is_some_and(|entry| entry.pattern.is_empty());\n    state\n        .list\n        .iter()\n        .enumerate()\n        .filter_map(|(index, entry)| {\n            let mut rect = RECT::default();\n            let result = unsafe {\n                send_message(\n                    state.listbox,\n                    LB_GETITEMRECT,\n                    WPARAM(index),\n                    LPARAM(&mut rect as *mut RECT as isize),\n                )\n            };\n            if result.0 < 0 {\n                return None;\n            }\n            let checked = unsafe { send_message(state.listbox, LB_GETITEMDATA, WPARAM(index), LPARAM(0)) }.0 as usize\n                == BST_CHECKED;\n            let toggleable = !(fixed_status && index == 0);\n            let name = if toggleable {\n                entry.display_name.clone()\n            } else {\n                format!("{} (always included)", entry.display_name)\n            };\n            Some(PickerAccessibleRow {\n                index,\n                name,\n                checked,\n                toggleable,\n                rect,\n            })\n        })\n        .collect()\n}\n\npub(crate) fn picker_selected_index(parent: HWND) -> Option<usize> {\n    let state_ptr = window_state::<PickerState>(parent);\n    if state_ptr.is_null() {\n        return None;\n    }\n    let state = unsafe { &*state_ptr };\n    if state.listbox.0.is_null() {\n        return None;\n    }\n    let selected = unsafe { send_message(state.listbox, LB_GETCURSEL, WPARAM(0), LPARAM(0)) }.0;\n    (selected >= 0).then_some(selected as usize)\n}\n\n''',
)
replace(
    p,
    '''    match message {\n        WM_NCDESTROY => {\n            // Unhook cleanly before deflecting the rest of destruction.\n            let _ = unsafe { RemoveWindowSubclass(lb, Some(listbox_proc), LISTBOX_SUBCLASS_ID) };\n            unsafe { DefSubclassProc(lb, message, wparam, lparam) }\n        }\n''',
    '''    match message {\n        WM_GETOBJECT => {\n            if let Some(provider) = crate::accessibility::picker_provider(parent, lb) {\n                unsafe {\n                    windows::Win32::UI::Accessibility::UiaReturnRawElementProvider(\n                        lb,\n                        wparam,\n                        lparam,\n                        Some(&provider),\n                    )\n                }\n            } else {\n                unsafe { DefSubclassProc(lb, message, wparam, lparam) }\n            }\n        }\n        WM_NCDESTROY => {\n            // UIA core can retain providers across destruction; disconnect the\n            // listbox provider before removing our subclass so no later query\n            // can resolve window state that has already been released.\n            crate::accessibility::detach_hwnd_provider(lb);\n            let _ = unsafe { RemoveWindowSubclass(lb, Some(listbox_proc), LISTBOX_SUBCLASS_ID) };\n            unsafe { DefSubclassProc(lb, message, wparam, lparam) }\n        }\n''',
)
replace(
    p,
    "        WM_CREATE => LRESULT(0),\n",
    '''        WM_CREATE => LRESULT(0),\n        PICKER_UIA_ACTION_MSG => {\n            let state_ptr = window_state::<PickerState>(hwnd);\n            if !state_ptr.is_null() {\n                let state = unsafe { &mut *state_ptr };\n                let index = wparam.0;\n                if index < state.list.len() && !state.listbox.0.is_null() {\n                    let _ = unsafe { send_message(state.listbox, LB_SETCURSEL, WPARAM(index), LPARAM(0)) };\n                    let _ = unsafe { SetFocus(Some(state.listbox)) };\n                    if lparam.0 == 0 {\n                        toggle_picker_row(state.listbox, state, index);\n                    }\n                }\n            }\n            LRESULT(0)\n        }\n''',
)

# Keep the private-message collision inventory compiler-checked.
p = "src/events.rs"
replace(
    p,
    "pub(crate) const APP_PRIVATE_MESSAGE_IDS: [u32; 14] = [\n",
    "pub(crate) const APP_PRIVATE_MESSAGE_IDS: [u32; 15] = [\n",
)
replace(
    p,
    "    crate::process_picker::PINNED_SOURCE_RESULT_MSG,\n];\n",
    "    crate::process_picker::PINNED_SOURCE_RESULT_MSG,\n    crate::process_picker::PICKER_UIA_ACTION_MSG,\n];\n",
)
replace(
    p,
    "/// too, so the assertion covers the newest message class as it grows.\n",
    "/// too, and the picker UIA action message is tracked here as well.\n",
)

# Picker fragment provider: one UIA checkbox child per owner-drawn row.
p = "src/accessibility.rs"
replace(
    p,
    "    UIA_GroupControlTypeId, UIA_HasKeyboardFocusPropertyId, UIA_InvokePatternId, UIA_IsEnabledPropertyId,\n",
    "    UIA_CheckBoxControlTypeId, UIA_GroupControlTypeId, UIA_HasKeyboardFocusPropertyId, UIA_InvokePatternId, UIA_IsEnabledPropertyId,\n",
)
insert_before(
    p,
    "/// Read-only name provider for the passive overlay pill window.\n",
    '''/// UI Automation fragment for the owner-drawn app-picker listbox. The\n/// native LISTBOX exposes selection, but its custom checkbox state lives in\n/// item data and is otherwise invisible to UIA. This provider exposes each row\n/// as a CheckBox with a live ToggleState and routes Toggle/SetFocus back to the\n/// picker UI thread through `PICKER_UIA_ACTION_MSG`.\n#[derive(Clone)]\nenum PickerProviderKind {\n    Root,\n    Child(usize),\n}\n\n#[implement(\n    IRawElementProviderSimple,\n    IRawElementProviderFragment,\n    IRawElementProviderFragmentRoot,\n    IToggleProvider\n)]\nstruct PickerProvider {\n    parent: HWND,\n    listbox: HWND,\n    kind: PickerProviderKind,\n}\n\nimpl PickerProvider {\n    fn resolve(&self) -> Option<crate::process_picker::PickerAccessibleRow> {\n        let PickerProviderKind::Child(index) = self.kind else {\n            return None;\n        };\n        crate::process_picker::picker_accessibility_rows(self.parent)\n            .into_iter()\n            .find(|row| row.index == index)\n    }\n\n    fn make(&self, kind: PickerProviderKind) -> PickerProvider {\n        PickerProvider {\n            parent: self.parent,\n            listbox: self.listbox,\n            kind,\n        }\n    }\n\n    fn child_fragment(&self, index: usize) -> IRawElementProviderFragment {\n        self.make(PickerProviderKind::Child(index)).into()\n    }\n\n    fn root_fragment(&self) -> IRawElementProviderFragment {\n        self.make(PickerProviderKind::Root).into()\n    }\n\n    fn root_fragment_root(&self) -> IRawElementProviderFragmentRoot {\n        self.make(PickerProviderKind::Root).into()\n    }\n\n    fn screen_rect(&self, client: RECT) -> UiaRect {\n        let mut p = POINT {\n            x: client.left,\n            y: client.top,\n        };\n        if !self.listbox.0.is_null() {\n            unsafe {\n                let _ = ClientToScreen(self.listbox, &mut p);\n            }\n        }\n        UiaRect {\n            left: p.x as f64,\n            top: p.y as f64,\n            width: (client.right - client.left) as f64,\n            height: (client.bottom - client.top) as f64,\n        }\n    }\n\n    fn activate(&self, action: isize) {\n        let Some(row) = self.resolve() else {\n            return;\n        };\n        if self.parent.0.is_null() {\n            return;\n        }\n        let _ = unsafe {\n            post_message(\n                self.parent,\n                crate::process_picker::PICKER_UIA_ACTION_MSG,\n                WPARAM(row.index),\n                LPARAM(action),\n            )\n        };\n    }\n}\n\nimpl IRawElementProviderSimple_Impl for PickerProvider_Impl {\n    fn ProviderOptions(&self) -> windows::core::Result<windows::Win32::UI::Accessibility::ProviderOptions> {\n        catch_uia("picker UIA ProviderOptions", || Ok(ProviderOptions_ServerSideProvider))\n    }\n\n    fn GetPatternProvider(&self, patternid: UIA_PATTERN_ID) -> windows::core::Result<IUnknown> {\n        catch_uia("picker UIA GetPatternProvider", || {\n            let this = &self.this;\n            let Some(row) = this.resolve() else {\n                return Err(Error::empty());\n            };\n            if row.toggleable && patternid == UIA_TogglePatternId {\n                let p: IToggleProvider = this.make(PickerProviderKind::Child(row.index)).into();\n                return p.cast::<IUnknown>();\n            }\n            Err(Error::empty())\n        })\n    }\n\n    fn GetPropertyValue(&self, propertyid: UIA_PROPERTY_ID) -> windows::core::Result<VARIANT> {\n        catch_uia("picker UIA GetPropertyValue", || {\n            let this = &self.this;\n            if propertyid == UIA_NamePropertyId {\n                let name = match this.kind {\n                    PickerProviderKind::Root => "App selection".to_string(),\n                    PickerProviderKind::Child(_) => this.resolve().map(|row| row.name).unwrap_or_default(),\n                };\n                return Ok(VARIANT::from(BSTR::from(name)));\n            }\n            if propertyid == windows::Win32::UI::Accessibility::UIA_ControlTypePropertyId {\n                let ty = if matches!(this.kind, PickerProviderKind::Root) {\n                    UIA_PaneControlTypeId\n                } else {\n                    UIA_CheckBoxControlTypeId\n                };\n                return Ok(VARIANT::from(ty.0));\n            }\n            if propertyid == UIA_IsEnabledPropertyId {\n                let enabled = match this.kind {\n                    PickerProviderKind::Root => true,\n                    PickerProviderKind::Child(_) => this.resolve().is_some(),\n                };\n                return Ok(VARIANT::from(enabled));\n            }\n            if propertyid == UIA_IsKeyboardFocusablePropertyId {\n                return Ok(VARIANT::from(matches!(this.kind, PickerProviderKind::Child(_))));\n            }\n            if propertyid == UIA_HasKeyboardFocusPropertyId {\n                let focused = match this.kind {\n                    PickerProviderKind::Child(index) => crate::process_picker::picker_selected_index(this.parent) == Some(index),\n                    PickerProviderKind::Root => false,\n                };\n                return Ok(VARIANT::from(focused));\n            }\n            Ok(VARIANT::default())\n        })\n    }\n\n    fn HostRawElementProvider(&self) -> windows::core::Result<IRawElementProviderSimple> {\n        catch_uia("picker UIA HostRawElementProvider", || {\n            if matches!(self.this.kind, PickerProviderKind::Root) && !self.this.listbox.0.is_null() {\n                unsafe { UiaHostProviderFromHwnd(self.this.listbox) }\n            } else {\n                Err(Error::empty())\n            }\n        })\n    }\n}\n\nimpl IRawElementProviderFragment_Impl for PickerProvider_Impl {\n    fn Navigate(&self, direction: NavigateDirection) -> windows::core::Result<IRawElementProviderFragment> {\n        catch_uia("picker UIA Navigate", || {\n            let this = &self.this;\n            let rows = crate::process_picker::picker_accessibility_rows(this.parent);\n            match this.kind {\n                PickerProviderKind::Root => {\n                    if direction == NavigateDirection_FirstChild && let Some(row) = rows.first() {\n                        return Ok(this.child_fragment(row.index));\n                    }\n                    if direction == NavigateDirection_LastChild && let Some(row) = rows.last() {\n                        return Ok(this.child_fragment(row.index));\n                    }\n                }\n                PickerProviderKind::Child(index) => {\n                    if direction == NavigateDirection_Parent {\n                        return Ok(this.root_fragment());\n                    }\n                    if let Some(pos) = rows.iter().position(|row| row.index == index) {\n                        if direction == NavigateDirection_NextSibling && let Some(row) = rows.get(pos + 1) {\n                            return Ok(this.child_fragment(row.index));\n                        }\n                        if direction == NavigateDirection_PreviousSibling && pos > 0 {\n                            return Ok(this.child_fragment(rows[pos - 1].index));\n                        }\n                    }\n                }\n            }\n            Err(Error::empty())\n        })\n    }\n\n    fn GetRuntimeId(&self) -> windows::core::Result<*mut SAFEARRAY> {\n        catch_uia("picker UIA GetRuntimeId", || match self.this.kind {\n            PickerProviderKind::Root => Ok(std::ptr::null_mut()),\n            PickerProviderKind::Child(index) => runtime_id_array(0x6000_i32.saturating_add(index as i32)),\n        })\n    }\n\n    fn BoundingRectangle(&self) -> windows::core::Result<UiaRect> {\n        catch_uia("picker UIA BoundingRectangle", || {\n            let this = &self.this;\n            let client = match this.kind {\n                PickerProviderKind::Root => {\n                    let mut rect = RECT::default();\n                    if this.listbox.0.is_null() {\n                        None\n                    } else {\n                        unsafe {\n                            let _ = windows::Win32::UI::WindowsAndMessaging::GetClientRect(this.listbox, &mut rect);\n                        }\n                        Some(rect)\n                    }\n                }\n                PickerProviderKind::Child(_) => this.resolve().map(|row| row.rect),\n            };\n            Ok(client.map_or(UiaRect::default(), |rect| this.screen_rect(rect)))\n        })\n    }\n\n    fn GetEmbeddedFragmentRoots(&self) -> windows::core::Result<*mut SAFEARRAY> {\n        catch_uia("picker UIA GetEmbeddedFragmentRoots", || Ok(std::ptr::null_mut()))\n    }\n\n    fn SetFocus(&self) -> windows::core::Result<()> {\n        catch_uia("picker UIA SetFocus", || {\n            if matches!(self.this.kind, PickerProviderKind::Child(_)) {\n                self.this.activate(1);\n            }\n            Ok(())\n        })\n    }\n\n    fn FragmentRoot(&self) -> windows::core::Result<IRawElementProviderFragmentRoot> {\n        catch_uia("picker UIA FragmentRoot", || Ok(self.this.root_fragment_root()))\n    }\n}\n\nimpl IRawElementProviderFragmentRoot_Impl for PickerProvider_Impl {\n    fn ElementProviderFromPoint(&self, x: f64, y: f64) -> windows::core::Result<IRawElementProviderFragment> {\n        catch_uia("picker UIA ElementProviderFromPoint", || {\n            let this = &self.this;\n            let mut point = POINT { x: x as i32, y: y as i32 };\n            if !this.listbox.0.is_null() {\n                unsafe {\n                    let _ = ScreenToClient(this.listbox, &mut point);\n                }\n            }\n            for row in crate::process_picker::picker_accessibility_rows(this.parent) {\n                if point.x >= row.rect.left && point.x < row.rect.right && point.y >= row.rect.top && point.y < row.rect.bottom {\n                    return Ok(this.child_fragment(row.index));\n                }\n            }\n            Ok(this.root_fragment())\n        })\n    }\n\n    fn GetFocus(&self) -> windows::core::Result<IRawElementProviderFragment> {\n        catch_uia("picker UIA GetFocus", || {\n            let Some(index) = crate::process_picker::picker_selected_index(self.this.parent) else {\n                return Err(Error::empty());\n            };\n            Ok(self.this.child_fragment(index))\n        })\n    }\n}\n\nimpl IToggleProvider_Impl for PickerProvider_Impl {\n    fn Toggle(&self) -> windows::core::Result<()> {\n        catch_uia("picker UIA Toggle", || {\n            if self.this.resolve().is_some_and(|row| row.toggleable) {\n                self.this.activate(0);\n            }\n            Ok(())\n        })\n    }\n\n    fn ToggleState(&self) -> windows::core::Result<windows::Win32::UI::Accessibility::ToggleState> {\n        catch_uia("picker UIA ToggleState", || {\n            Ok(if self.this.resolve().is_some_and(|row| row.checked) {\n                ToggleState_On\n            } else {\n                ToggleState_Off\n            })\n        })\n    }\n}\n\npub fn picker_provider(parent: HWND, listbox: HWND) -> Option<IRawElementProviderSimple> {\n    if crate::process_picker::picker_accessibility_rows(parent).is_empty() {\n        return None;\n    }\n    Some(\n        PickerProvider {\n            parent,\n            listbox,\n            kind: PickerProviderKind::Root,\n        }\n        .into(),\n    )\n}\n\n''',
)

# Explain effective rendering without mutating persisted custom color.
p = "docs/configuration.md"
replace(
    p,
    "| `text_color`       | `[255, 255, 255, 255]` | RGBA 0–255 | Title and state-label color |",
    "| `text_color`       | `[255, 255, 255, 255]` | RGBA 0–255 | Stored title/state color; the Activity title is rendered with the minimum lightening needed for 4.5:1 contrast on its black surface |",
)

subprocess.run(["cargo", "fmt", "--all"], check=True)
subprocess.run(["git", "config", "user.name", "github-actions[bot]"], check=True)
subprocess.run(["git", "config", "user.email", "41898282+github-actions[bot]@users.noreply.github.com"], check=True)
subprocess.run(["git", "add", "src/main_window.rs", "src/process_picker.rs", "src/accessibility.rs", "src/events.rs", "docs/configuration.md"], check=True)
subprocess.run(["git", "commit", "-m", "fix(accessibility): correct focus, contrast, and picker semantics"], check=True)
subprocess.run(["git", "push", "origin", "HEAD:checkpoint"], check=True)
