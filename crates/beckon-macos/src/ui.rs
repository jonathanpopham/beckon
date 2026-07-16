//! The keyboard-driven results list: an NSTableView in an NSScrollView
//! under the query field, plus the field delegate that gives the launcher
//! its feel. The query field keeps keyboard focus at all times; the table
//! is display only (refusesFirstResponder) and every keystroke that means
//! "navigate the list" is intercepted in the field delegate's
//! control:textView:doCommandBySelector: hook, the same hook that already
//! handles Escape.
//!
//! Keyboard model:
//!   Up / Down    move the selection; the selection wraps at the ends
//!                (Down from the last row goes to row 0, Up from row 0
//!                goes to the last row)
//!   Return       fires the activation callback with the selected index
//!   Escape       hides the panel (unchanged from panel.rs behavior)
//!
//! Threading invariants: everything here is main-thread only. AppKit is
//! main-thread only (ffi module invariant 2), so install(), set_items(),
//! selected_index(), move_selection(), activate_selected(), and both
//! callbacks all run on the main thread; the delegate and data source
//! methods are only ever invoked by the main run loop. The Mutex and
//! OnceLock statics exist to satisfy Rust's static-safety rules, not to
//! enable cross-thread use. Callbacks are invoked with their own slot
//! locked, so a callback must not call its own set_on_* function; calling
//! set_items(), selected_index(), or panel functions from a callback is
//! fine (they use separate locks or none).

use crate::ffi::{self, msg, Bool, Id, ObjcObject, Sel, NIL, NO, YES};
use crate::{panel, theme};
use std::mem::transmute;
use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::{Mutex, Once, OnceLock};

/// One row of the results list. The title renders in the theme's
/// foreground color (accent when the row is selected); the subtitle is
/// appended after two spaces and rendered dimmed. Without an applied
/// theme (theme::row_style() is None) rows keep the built-in white.
#[derive(Clone, Debug, Default)]
pub struct RowData {
    pub title: String,
    pub subtitle: String,
}

type QueryCallback = Box<dyn Fn(String) + Send>;
type ActivateCallback = Box<dyn Fn(usize) + Send>;

static ITEMS: OnceLock<Mutex<Vec<RowData>>> = OnceLock::new();
static ON_QUERY_CHANGED: OnceLock<Mutex<Option<QueryCallback>>> = OnceLock::new();
static ON_ACTIVATE: OnceLock<Mutex<Option<ActivateCallback>>> = OnceLock::new();

static TABLE: AtomicPtr<ObjcObject> = AtomicPtr::new(ptr::null_mut());
static DATA_SOURCE: AtomicPtr<ObjcObject> = AtomicPtr::new(ptr::null_mut());
static FIELD_DELEGATE: AtomicPtr<ObjcObject> = AtomicPtr::new(ptr::null_mut());
static DEFINE_DS: Once = Once::new();
static DEFINE_DELEGATE: Once = Once::new();

// Layout inside the panel's content view (bottom-left origin coordinates).
const LIST_MARGIN: f64 = 16.0;
// The query field sits at y = PANEL_HEIGHT - 64 (see panel.rs); the list
// fills the space beneath it with a small gap.
const LIST_TOP_GAP: f64 = 12.0;
const ROW_HEIGHT: f64 = 30.0;
// NSTableViewStylePlain: no automatic inset, matches the panel's own
// padding. Guarded by respondsToSelector: (the setter is macOS 11+).
const TABLE_STYLE_PLAIN: isize = 4;

fn items() -> &'static Mutex<Vec<RowData>> {
    ITEMS.get_or_init(|| Mutex::new(Vec::new()))
}

fn on_query_changed() -> &'static Mutex<Option<QueryCallback>> {
    ON_QUERY_CHANGED.get_or_init(|| Mutex::new(None))
}

fn on_activate() -> &'static Mutex<Option<ActivateCallback>> {
    ON_ACTIVATE.get_or_init(|| Mutex::new(None))
}

// ---------------------------------------------------------------------------
// Data source: a runtime class implementing the two cell-based
// NSTableViewDataSource methods. A programmatically created NSTableView
// with no view-based delegate methods renders through the column's
// dataCell, which is exactly the simple path we want.
// ---------------------------------------------------------------------------

extern "C" fn number_of_rows(_this: Id, _sel: Sel, _table: Id) -> isize {
    items().lock().unwrap().len() as isize
}

/// The table's doubleAction: a double click activates the clicked row,
/// exactly like Return on it. Clicking already moved the selection, so
/// the shared activation path applies.
extern "C" fn row_double_clicked(_this: Id, _sel: Sel, _sender: Id) {
    activate_selected();
}

extern "C" fn object_value(_this: Id, _sel: Sel, table: Id, _column: Id, row: isize) -> Id {
    let guard = items().lock().unwrap();
    let Some(data) = usize::try_from(row).ok().and_then(|i| guard.get(i)) else {
        return NIL;
    };
    // Cell-based tables redraw the affected rows on every selection
    // change and drawing re-asks the data source, so tinting the
    // selected row's title here tracks the selection exactly.
    // Safety: main thread (the table only asks for values from the run
    // loop or from set_items/reloadData, both main thread); selectedRow
    // returns NSInteger; signatures in row_value match AppKit.
    unsafe {
        let selected = !table.is_null() && msg!(isize: table, ffi::sel("selectedRow")) == row;
        row_value(data, selected)
    }
}

/// Build an autoreleased attributed string for one row: the title in the
/// theme foreground (accent when selected, the wiring theme.rs documents
/// for the selection highlight), then the subtitle after two spaces in a
/// dimmed variant of the foreground. Row fonts scale with the theme's
/// font_size relative to the default 22pt, so the default theme keeps
/// the built-in 15pt/13pt look. The attribute keys are the documented
/// literal values of NSForegroundColorAttributeName and
/// NSFontAttributeName ("NSColor" and "NSFont"), spelled out because we
/// link no headers to import the constants from.
///
/// # Safety
/// Main thread; every msg! spells the documented AppKit signature.
unsafe fn row_value(data: &RowData, selected: bool) -> Id {
    // Before theme::apply runs (tests, headless), keep the built-in
    // white-on-dark look.
    let (fg, accent, scale) = match theme::row_style() {
        Some(s) => (s.foreground, s.accent, f64::from(s.font_size) / 22.0),
        None => ((255, 255, 255), (255, 255, 255), 1.0),
    };
    let title_font = msg!(Id: ffi::class("NSFont"), ffi::sel("systemFontOfSize:"),
        f64: 15.0 * scale);
    let title_color = rgb_color(if selected { accent } else { fg }, 1.0);
    let value = msg!(Id: msg!(Id: ffi::class("NSMutableAttributedString"), ffi::sel("alloc")),
        ffi::sel("initWithString:attributes:"),
        Id: ffi::nsstring(&data.title),
        Id: attributes(title_color, title_font));
    if !data.subtitle.is_empty() {
        let sub_font = msg!(Id: ffi::class("NSFont"), ffi::sel("systemFontOfSize:"),
            f64: 13.0 * scale);
        let dim = rgb_color(fg, 0.45);
        let sub = msg!(Id: msg!(Id: ffi::class("NSAttributedString"), ffi::sel("alloc")),
            ffi::sel("initWithString:attributes:"),
            Id: ffi::nsstring(&format!("  {}", data.subtitle)),
            Id: attributes(dim, sub_font));
        msg!((): value, ffi::sel("appendAttributedString:"), Id: sub);
        // Balance the alloc; the mutable string copied what it needed.
        msg!((): sub, ffi::sel("release"));
    }
    // Autorelease: this is called on every redraw, so leaking here would
    // grow without bound. The run loop's pool drains it.
    msg!(Id: value, ffi::sel("autorelease"))
}

/// Autoreleased NSColor from theme channels (0..=255) plus an alpha.
///
/// # Safety
/// Main thread; colorWithCalibratedRed:green:blue:alpha: takes four
/// CGFloats and returns an autoreleased NSColor.
unsafe fn rgb_color(rgb: (u8, u8, u8), alpha: f64) -> Id {
    msg!(Id: ffi::class("NSColor"),
        ffi::sel("colorWithCalibratedRed:green:blue:alpha:"),
        f64: f64::from(rgb.0) / 255.0,
        f64: f64::from(rgb.1) / 255.0,
        f64: f64::from(rgb.2) / 255.0,
        f64: alpha)
}

/// Autoreleased attribute dictionary {NSColor: color, NSFont: font}.
///
/// # Safety
/// Main thread; color and font must be valid NSColor / NSFont instances.
unsafe fn attributes(color: Id, font: Id) -> Id {
    let dict = msg!(Id: ffi::class("NSMutableDictionary"), ffi::sel("dictionary"));
    msg!((): dict, ffi::sel("setObject:forKey:"), Id: color, Id: ffi::nsstring("NSColor"));
    msg!((): dict, ffi::sel("setObject:forKey:"), Id: font, Id: ffi::nsstring("NSFont"));
    dict
}

/// Define the data source class once and return its single instance.
fn data_source() -> Id {
    DEFINE_DS.call_once(|| {
        // Safety: each Imp is transmuted from an extern "C" fn whose real
        // signature matches the paired encoding; the class name is
        // registered exactly once (guarded by the Once); instance creation
        // is plain alloc/init on NSObject.
        let instance = unsafe {
            let cls = ffi::define_class(
                "BeckonDataSource",
                "NSObject",
                &[
                    (
                        "numberOfRowsInTableView:",
                        transmute::<extern "C" fn(Id, Sel, Id) -> isize, ffi::Imp>(number_of_rows),
                        "q@:@",
                    ),
                    (
                        "tableView:objectValueForTableColumn:row:",
                        transmute::<extern "C" fn(Id, Sel, Id, Id, isize) -> Id, ffi::Imp>(
                            object_value,
                        ),
                        "@@:@@q",
                    ),
                    (
                        "beckonRowDoubleClicked:",
                        transmute::<extern "C" fn(Id, Sel, Id), ffi::Imp>(row_double_clicked),
                        "v@:@",
                    ),
                ],
            );
            msg!(Id: msg!(Id: cls, ffi::sel("alloc")), ffi::sel("init"))
        };
        DATA_SOURCE.store(instance, Ordering::Relaxed);
    });
    DATA_SOURCE.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Field delegate: the keyboard model. One runtime class, one instance,
// installed as the query field's delegate by panel::init.
// ---------------------------------------------------------------------------

/// NSTextFieldDelegate hook: while the field editor has focus, command
/// selectors (Escape, arrows, Return) arrive here before anything else.
/// Returning YES swallows the command so the field editor never sees it.
extern "C" fn control_do_command(
    _this: Id,
    _sel: Sel,
    _control: Id,
    _text_view: Id,
    command: Sel,
) -> Bool {
    if command == ffi::sel("cancelOperation:") {
        panel::hide();
        YES
    } else if command == ffi::sel("moveUp:") {
        move_selection(-1);
        YES
    } else if command == ffi::sel("moveDown:") {
        move_selection(1);
        YES
    } else if command == ffi::sel("insertNewline:") {
        activate_selected();
        YES
    } else {
        NO
    }
}

/// NSTextFieldDelegate hook: fires on every user edit (the control posts
/// NSControlTextDidChangeNotification and NSControl auto-subscribes its
/// delegate). Reads the field and forwards to the registered callback.
extern "C" fn control_text_did_change(_this: Id, _sel: Sel, _notification: Id) {
    notify_query_changed();
}

/// Read the query field and invoke the query-changed callback with it.
/// Called by the delegate; also callable directly (the smoke test drives
/// it through the real notification path instead).
pub fn notify_query_changed() {
    let query = panel::query();
    let guard = on_query_changed().lock().unwrap();
    if let Some(cb) = guard.as_ref() {
        cb(query);
    }
}

/// Define the field delegate class once and return its single instance.
/// panel::init installs this as the query field's delegate.
pub fn field_delegate() -> Id {
    DEFINE_DELEGATE.call_once(|| {
        // Safety: as in data_source(); encodings match the real signatures
        // and the class is registered exactly once.
        let instance = unsafe {
            let cls = ffi::define_class(
                "BeckonFieldDelegate",
                "NSObject",
                &[
                    (
                        "control:textView:doCommandBySelector:",
                        transmute::<extern "C" fn(Id, Sel, Id, Id, Sel) -> Bool, ffi::Imp>(
                            control_do_command,
                        ),
                        "c@:@@:",
                    ),
                    (
                        "controlTextDidChange:",
                        transmute::<extern "C" fn(Id, Sel, Id), ffi::Imp>(control_text_did_change),
                        "v@:@",
                    ),
                ],
            );
            msg!(Id: msg!(Id: cls, ffi::sel("alloc")), ffi::sel("init"))
        };
        FIELD_DELEGATE.store(instance, Ordering::Relaxed);
    });
    FIELD_DELEGATE.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// View construction
// ---------------------------------------------------------------------------

/// Build the scroll view and table under the query field and add them to
/// the panel's content view. Called once by panel::init, main thread,
/// before the run loop starts.
pub fn install(content: Id) {
    let ds = data_source();
    // Safety: main thread, called once at startup; every msg! spells the
    // documented AppKit signature (ffi invariant 1). The scroll view,
    // table, and column live as long as the process (ffi invariant 3).
    unsafe {
        let width = panel::PANEL_WIDTH - 2.0 * LIST_MARGIN;
        let height = (panel::PANEL_HEIGHT - 64.0) - LIST_TOP_GAP - LIST_MARGIN;
        let list_rect = ffi::NSRect::new(LIST_MARGIN, LIST_MARGIN, width, height);

        let scroll = msg!(Id: msg!(Id: ffi::class("NSScrollView"), ffi::sel("alloc")),
            ffi::sel("initWithFrame:"), ffi::NSRect: list_rect);
        assert!(!scroll.is_null(), "NSScrollView init returned nil");
        msg!((): scroll, ffi::sel("setDrawsBackground:"), Bool: NO);
        msg!((): scroll, ffi::sel("setHasVerticalScroller:"), Bool: YES);
        msg!((): scroll, ffi::sel("setAutohidesScrollers:"), Bool: YES);
        // NSNoBorder.
        msg!((): scroll, ffi::sel("setBorderType:"), usize: 0);

        let table_rect = ffi::NSRect::new(0.0, 0.0, width, height);
        let table = msg!(Id: msg!(Id: ffi::class("NSTableView"), ffi::sel("alloc")),
            ffi::sel("initWithFrame:"), ffi::NSRect: table_rect);
        assert!(!table.is_null(), "NSTableView init returned nil");
        msg!((): table, ffi::sel("setHeaderView:"), Id: NIL);
        let clear = msg!(Id: ffi::class("NSColor"), ffi::sel("clearColor"));
        msg!((): table, ffi::sel("setBackgroundColor:"), Id: clear);
        msg!((): table, ffi::sel("setRowHeight:"), f64: ROW_HEIGHT);
        msg!((): table, ffi::sel("setAllowsMultipleSelection:"), Bool: NO);
        msg!((): table, ffi::sel("setAllowsEmptySelection:"), Bool: YES);
        // The launcher feel: the table never takes keyboard focus, so the
        // query field keeps the caret while arrows drive the selection.
        msg!((): table, ffi::sel("setRefusesFirstResponder:"), Bool: YES);
        if msg!(Bool: table, ffi::sel("respondsToSelector:"), Sel: ffi::sel("setStyle:")) != 0 {
            msg!((): table, ffi::sel("setStyle:"), isize: TABLE_STYLE_PLAIN);
        }

        let column = msg!(Id: msg!(Id: ffi::class("NSTableColumn"), ffi::sel("alloc")),
            ffi::sel("initWithIdentifier:"), Id: ffi::nsstring("beckon.results"));
        assert!(!column.is_null(), "NSTableColumn init returned nil");
        msg!((): column, ffi::sel("setWidth:"), f64: width - 4.0);
        // Rows are results, not documents: a double click must never
        // drop a field editor into the cell (setEditable:NO on both the
        // column and its cell); it activates the row instead, matching
        // Return.
        msg!((): column, ffi::sel("setEditable:"), Bool: NO);
        let cell = msg!(Id: column, ffi::sel("dataCell"));
        msg!((): cell, ffi::sel("setEditable:"), Bool: NO);
        msg!((): cell, ffi::sel("setSelectable:"), Bool: NO);
        msg!((): table, ffi::sel("addTableColumn:"), Id: column);
        msg!((): table, ffi::sel("setTarget:"), Id: ds);
        msg!((): table, ffi::sel("setDoubleAction:"), Sel: ffi::sel("beckonRowDoubleClicked:"));

        msg!((): table, ffi::sel("setDataSource:"), Id: ds);
        msg!((): scroll, ffi::sel("setDocumentView:"), Id: table);
        msg!((): content, ffi::sel("addSubview:"), Id: scroll);

        TABLE.store(table, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// Rust-side API (what the engine wiring calls)
// ---------------------------------------------------------------------------

/// Replace the list contents, reload the table, and reset the selection
/// to row 0 (or clear it when the list is empty). Main thread only.
pub fn set_items(rows: &[RowData]) {
    {
        // Scope the lock: reloadData synchronously calls the data source,
        // which locks ITEMS again; holding it here would deadlock.
        let mut guard = items().lock().unwrap();
        guard.clear();
        guard.extend_from_slice(rows);
    }
    let table = TABLE.load(Ordering::Relaxed);
    if table.is_null() {
        return;
    }
    // Safety: main thread; reloadData takes no arguments.
    unsafe {
        msg!((): table, ffi::sel("reloadData"));
        select_row(if rows.is_empty() { None } else { Some(0) });
    }
}

/// The currently selected row, or None when the list is empty or nothing
/// is selected. Reads the table's own selection state. Main thread only.
pub fn selected_index() -> Option<usize> {
    let table = TABLE.load(Ordering::Relaxed);
    if table.is_null() {
        return None;
    }
    // Safety: main thread; selectedRow returns NSInteger (-1 for none).
    let row = unsafe { msg!(isize: table, ffi::sel("selectedRow")) };
    usize::try_from(row).ok()
}

/// Register the query-changed callback. Invoked on the main thread with
/// the field's full current text on every edit. Replaces any previous
/// callback. Must not be called from inside the callback itself.
pub fn set_on_query_changed(cb: impl Fn(String) + Send + 'static) {
    *on_query_changed().lock().unwrap() = Some(Box::new(cb));
}

/// Register the activation callback. Invoked on the main thread with the
/// selected row index when Return is pressed. Replaces any previous
/// callback. Must not be called from inside the callback itself.
pub fn set_on_activate(cb: impl Fn(usize) + Send + 'static) {
    *on_activate().lock().unwrap() = Some(Box::new(cb));
}

/// Move the selection by delta rows, wrapping at the ends (see the module
/// docs). With no current selection, Down selects row 0 and Up selects
/// the last row. Main thread only.
pub fn move_selection(delta: isize) {
    let rows = items().lock().unwrap().len() as isize;
    if rows == 0 {
        return;
    }
    let table = TABLE.load(Ordering::Relaxed);
    if table.is_null() {
        return;
    }
    // Safety: main thread; selectedRow returns NSInteger.
    let current = unsafe { msg!(isize: table, ffi::sel("selectedRow")) };
    let next = if current < 0 {
        if delta >= 0 {
            0
        } else {
            rows - 1
        }
    } else {
        (current + delta).rem_euclid(rows)
    };
    // Safety: main thread; next is within 0..rows.
    unsafe { select_row(Some(next as usize)) };
}

/// Fire the activation callback with the selected index, if any.
pub fn activate_selected() {
    if let Some(index) = selected_index() {
        let guard = on_activate().lock().unwrap();
        if let Some(cb) = guard.as_ref() {
            cb(index);
        }
    }
}

/// Copy of the row at `index`, or None past the end. Main thread only;
/// the smoke test uses this to assert what the table is showing.
pub fn row_at(index: usize) -> Option<RowData> {
    items().lock().unwrap().get(index).cloned()
}

/// Ask the table itself how many rows it has; this round-trips through
/// the data source, which is what the smoke test wants to prove.
pub fn row_count() -> usize {
    let table = TABLE.load(Ordering::Relaxed);
    if table.is_null() {
        return 0;
    }
    // Safety: main thread; numberOfRows returns NSInteger.
    let n = unsafe { msg!(isize: table, ffi::sel("numberOfRows")) };
    usize::try_from(n).unwrap_or(0)
}

/// Select a row (scrolling it visible) or clear the selection.
///
/// # Safety
/// Main thread; `row`, when Some, must be less than the table's row count.
unsafe fn select_row(row: Option<usize>) {
    let table = TABLE.load(Ordering::Relaxed);
    if table.is_null() {
        return;
    }
    match row {
        Some(r) => {
            let set = msg!(Id: ffi::class("NSIndexSet"),
                ffi::sel("indexSetWithIndex:"), usize: r);
            msg!((): table, ffi::sel("selectRowIndexes:byExtendingSelection:"),
                Id: set, Bool: NO);
            msg!((): table, ffi::sel("scrollRowToVisible:"), isize: r as isize);
        }
        None => msg!((): table, ffi::sel("deselectAll:"), Id: NIL),
    }
}
