//! beckon: a keyboard launcher for macOS. Free, open source, airgap pure.
//!
//! This crate is the platform shell: it owns the floating panel, the global
//! hotkey, the indexers, and the Accessibility integrations, all through
//! hand-rolled Objective-C runtime FFI (no wrapper crates). All decisions
//! about what a query means and how results rank live in beckon-core.
//!
//! Flags: `--version` prints and exits; `--smoke` runs the automated shell
//! self-test (show the panel, round-trip an NSString, drive the results
//! list and its callbacks, hide, exit 0) with no user input, proving the
//! FFI end to end.
//!
//! On non-macOS targets this builds as a stub so the whole workspace
//! compiles and tests on Linux CI.

#[cfg(target_os = "macos")]
mod ffi;
#[cfg(target_os = "macos")]
mod hotkey;
#[cfg(target_os = "macos")]
mod panel;
#[cfg(target_os = "macos")]
mod ui;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("beckon {}", beckon_core::VERSION);
        return;
    }
    let smoke = args.iter().any(|a| a == "--smoke");

    #[cfg(target_os = "macos")]
    shell::run(smoke);

    #[cfg(not(target_os = "macos"))]
    {
        let _ = smoke;
        eprintln!(
            "beckon {} core built OK; the shell runs on macOS only",
            beckon_core::VERSION
        );
    }
}

#[cfg(target_os = "macos")]
mod shell {
    //! macOS wiring: NSApplication bootstrap, the app delegate, and the
    //! --smoke self-test. Feature logic lives in ffi.rs, panel.rs, and
    //! hotkey.rs; this module only glues them together.

    use crate::ffi::{self, msg, Bool, Id, Sel};
    use crate::ui::{self, RowData};
    use crate::{hotkey, panel};
    use std::mem::transmute;
    use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
    use std::sync::{Mutex, OnceLock};

    /// NSApplicationActivationPolicyAccessory: no Dock icon, no menu bar;
    /// the panel is the whole interface.
    const ACTIVATION_POLICY_ACCESSORY: isize = 1;

    static SMOKE: AtomicBool = AtomicBool::new(false);
    static SMOKE_SHOWED: AtomicBool = AtomicBool::new(false);
    static SMOKE_RESULTS: AtomicBool = AtomicBool::new(false);
    static SMOKE_HID: AtomicBool = AtomicBool::new(false);
    /// Last string the query-changed callback delivered.
    static SMOKE_QUERY: OnceLock<Mutex<String>> = OnceLock::new();
    /// Last index the activation callback delivered; -1 means never fired.
    static SMOKE_ACTIVATED: AtomicIsize = AtomicIsize::new(-1);

    fn smoke_query_slot() -> &'static Mutex<String> {
        SMOKE_QUERY.get_or_init(|| Mutex::new(String::new()))
    }

    pub fn run(smoke: bool) {
        SMOKE.store(smoke, Ordering::Relaxed);
        let _pool = ffi::AutoreleasePool::new();
        // Safety: main thread; every msg! spells the documented signature
        // of the NSApplication method it calls.
        unsafe {
            let app = msg!(Id: ffi::class("NSApplication"), ffi::sel("sharedApplication"));
            let _ = msg!(Bool: app, ffi::sel("setActivationPolicy:"),
                isize: ACTIVATION_POLICY_ACCESSORY);
            let delegate = make_delegate();
            // The query field's delegate (keyboard model) lives in ui.rs;
            // the app delegate here only handles lifecycle and smoke steps.
            panel::init(ui::field_delegate());
            msg!((): app, ffi::sel("setDelegate:"), Id: delegate);
            // Blocks until terminate; smoke mode exits from its last step.
            msg!((): app, ffi::sel("run"));
        }
    }

    /// Build the app delegate class and its single instance. It handles
    /// application lifecycle and the delayed smoke steps; the text field's
    /// delegate (Escape, arrows, Return, text changes) is ui.rs's job.
    ///
    /// # Safety
    /// Main thread, called once.
    unsafe fn make_delegate() -> Id {
        let cls = ffi::define_class(
            "BeckonAppDelegate",
            "NSObject",
            &[
                (
                    "applicationDidFinishLaunching:",
                    transmute::<extern "C" fn(Id, Sel, Id), ffi::Imp>(did_finish_launching),
                    "v@:@",
                ),
                (
                    "beckonSmokeShow:",
                    transmute::<extern "C" fn(Id, Sel, Id), ffi::Imp>(smoke_show),
                    "v@:@",
                ),
                (
                    "beckonSmokeResults:",
                    transmute::<extern "C" fn(Id, Sel, Id), ffi::Imp>(smoke_results),
                    "v@:@",
                ),
                (
                    "beckonSmokeHide:",
                    transmute::<extern "C" fn(Id, Sel, Id), ffi::Imp>(smoke_hide),
                    "v@:@",
                ),
                (
                    "beckonSmokeExit:",
                    transmute::<extern "C" fn(Id, Sel, Id), ffi::Imp>(smoke_exit),
                    "v@:@",
                ),
            ],
        );
        msg!(Id: msg!(Id: cls, ffi::sel("alloc")), ffi::sel("init"))
    }

    /// Schedule a zero-argument delegate selector on the main run loop.
    ///
    /// # Safety
    /// `obj` must respond to `selector` with encoding v@:@.
    unsafe fn perform_after(obj: Id, selector: &str, delay_seconds: f64) {
        msg!((): obj, ffi::sel("performSelector:withObject:afterDelay:"),
            Sel: ffi::sel(selector), Id: ffi::NIL, f64: delay_seconds);
    }

    /// The hotkey callback handed to Carbon; runs on the main thread.
    extern "C" fn hotkey_pressed() {
        panel::toggle();
    }

    extern "C" fn did_finish_launching(this: Id, _sel: Sel, _note: Id) {
        match hotkey::register(hotkey_pressed) {
            Ok(()) => println!("beckon: ready; press Option+Space to summon the panel"),
            Err(status) => {
                eprintln!("beckon: hotkey registration failed (OSStatus {status}); is Option+Space taken?");
            }
        }
        if SMOKE.load(Ordering::Relaxed) {
            // Safety: beckonSmokeShow: is defined on this delegate class
            // with encoding v@:@.
            unsafe { perform_after(this, "beckonSmokeShow:", 0.2) };
        }
    }

    extern "C" fn smoke_show(this: Id, _sel: Sel, _obj: Id) {
        panel::show();
        panel::set_query("beckon smoke");
        let round_trip = panel::query();
        let visible = panel::is_visible();
        let ok = visible && round_trip == "beckon smoke";
        SMOKE_SHOWED.store(ok, Ordering::Relaxed);
        println!(
            "smoke: shown visible={visible} nsstring_roundtrip={:?}",
            round_trip
        );
        // Safety: as in did_finish_launching.
        unsafe { perform_after(this, "beckonSmokeResults:", 0.2) };
    }

    /// Drive the results list end to end: rows through the data source,
    /// the query-changed callback through the real notification path, and
    /// selection movement plus activation through the real delegate hook.
    extern "C" fn smoke_results(this: Id, _sel: Sel, _obj: Id) {
        let mut ok = true;

        // Three fake rows; the table must report them via the data source
        // and the selection must reset to row 0.
        ui::set_items(&[
            RowData {
                title: "Calculator".into(),
                subtitle: "2+2 = 4".into(),
            },
            RowData {
                title: "Safari".into(),
                subtitle: "Application".into(),
            },
            RowData {
                title: "Lock Screen".into(),
                subtitle: "System".into(),
            },
        ]);
        let rows = ui::row_count();
        let initial = ui::selected_index();
        if rows != 3 || initial != Some(0) {
            ok = false;
        }
        println!("smoke: results rows={rows} initial_selection={initial:?}");

        // Query-changed callback: set the field text, then post the same
        // notification the field editor posts on user edits; NSControl
        // auto-subscribed the ui.rs delegate when panel::init set it.
        ui::set_on_query_changed(|q| *smoke_query_slot().lock().unwrap() = q);
        panel::set_query("calc 2+2");
        // Safety: main thread; postNotificationName:object: takes an
        // NSString name and a nullable object.
        unsafe {
            let center = msg!(Id: ffi::class("NSNotificationCenter"), ffi::sel("defaultCenter"));
            msg!((): center, ffi::sel("postNotificationName:object:"),
                Id: ffi::nsstring("NSControlTextDidChangeNotification"),
                Id: panel::field());
        }
        let seen = smoke_query_slot().lock().unwrap().clone();
        if seen != "calc 2+2" {
            ok = false;
        }
        println!("smoke: query_changed callback saw {seen:?}");

        // moveDown then Return through the delegate's real command hook.
        ui::set_on_activate(|i| SMOKE_ACTIVATED.store(i as isize, Ordering::Relaxed));
        let delegate = ui::field_delegate();
        // Safety: main thread; the delegate implements the method with
        // encoding c@:@@: and tolerates a nil text view.
        let (down_handled, return_handled) = unsafe {
            let down = msg!(Bool: delegate,
                ffi::sel("control:textView:doCommandBySelector:"),
                Id: panel::field(), Id: ffi::NIL, Sel: ffi::sel("moveDown:"));
            let ret = msg!(Bool: delegate,
                ffi::sel("control:textView:doCommandBySelector:"),
                Id: panel::field(), Id: ffi::NIL, Sel: ffi::sel("insertNewline:"));
            (down != 0, ret != 0)
        };
        let selected = ui::selected_index();
        let activated = SMOKE_ACTIVATED.load(Ordering::Relaxed);
        if !down_handled || !return_handled || selected != Some(1) || activated != 1 {
            ok = false;
        }
        println!(
            "smoke: moveDown handled={down_handled} selection={selected:?} \
             return handled={return_handled} activated_index={activated}"
        );

        SMOKE_RESULTS.store(ok, Ordering::Relaxed);
        println!("smoke: results {}", if ok { "OK" } else { "FAILED" });
        // Safety: as in did_finish_launching.
        unsafe { perform_after(this, "beckonSmokeHide:", 0.2) };
    }

    extern "C" fn smoke_hide(this: Id, _sel: Sel, _obj: Id) {
        panel::hide();
        let visible = panel::is_visible();
        SMOKE_HID.store(!visible, Ordering::Relaxed);
        println!("smoke: hidden visible={visible}");
        // Safety: as in did_finish_launching.
        unsafe { perform_after(this, "beckonSmokeExit:", 0.2) };
    }

    extern "C" fn smoke_exit(_this: Id, _sel: Sel, _obj: Id) {
        let ok = SMOKE_SHOWED.load(Ordering::Relaxed)
            && SMOKE_RESULTS.load(Ordering::Relaxed)
            && SMOKE_HID.load(Ordering::Relaxed);
        println!("smoke: {}", if ok { "PASS" } else { "FAIL" });
        std::process::exit(if ok { 0 } else { 1 });
    }
}
