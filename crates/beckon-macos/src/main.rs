//! beckon: a keyboard launcher for macOS. Free, open source, airgap pure.
//!
//! This crate is the platform shell: it owns the floating panel, the global
//! hotkey, the indexers, and the Accessibility integrations, all through
//! hand-rolled Objective-C runtime FFI (no wrapper crates). All decisions
//! about what a query means and how results rank live in beckon-core.
//!
//! Flags: `--version` prints and exits; `--smoke` runs the automated shell
//! self-test (show the panel, round-trip an NSString, hide, exit 0) with
//! no user input, proving the FFI end to end.
//!
//! On non-macOS targets this builds as a stub so the whole workspace
//! compiles and tests on Linux CI.

#[cfg(target_os = "macos")]
mod ffi;
#[cfg(target_os = "macos")]
mod hotkey;
#[cfg(target_os = "macos")]
mod panel;

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

    use crate::ffi::{self, msg, Bool, Id, Sel, NO, YES};
    use crate::{hotkey, panel};
    use std::mem::transmute;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// NSApplicationActivationPolicyAccessory: no Dock icon, no menu bar;
    /// the panel is the whole interface.
    const ACTIVATION_POLICY_ACCESSORY: isize = 1;

    static SMOKE: AtomicBool = AtomicBool::new(false);
    static SMOKE_SHOWED: AtomicBool = AtomicBool::new(false);
    static SMOKE_HID: AtomicBool = AtomicBool::new(false);

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
            panel::init(delegate);
            msg!((): app, ffi::sel("setDelegate:"), Id: delegate);
            // Blocks until terminate; smoke mode exits from its last step.
            msg!((): app, ffi::sel("run"));
        }
    }

    /// Build the app delegate class and its single instance. The same
    /// object is the text field's delegate so Escape can be intercepted
    /// while the field editor has focus.
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
                    "beckonSmokeHide:",
                    transmute::<extern "C" fn(Id, Sel, Id), ffi::Imp>(smoke_hide),
                    "v@:@",
                ),
                (
                    "beckonSmokeExit:",
                    transmute::<extern "C" fn(Id, Sel, Id), ffi::Imp>(smoke_exit),
                    "v@:@",
                ),
                (
                    "control:textView:doCommandBySelector:",
                    transmute::<extern "C" fn(Id, Sel, Id, Id, Sel) -> Bool, ffi::Imp>(
                        control_do_command,
                    ),
                    "c@:@@:",
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
        unsafe { perform_after(this, "beckonSmokeHide:", 0.6) };
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
        let ok = SMOKE_SHOWED.load(Ordering::Relaxed) && SMOKE_HID.load(Ordering::Relaxed);
        println!("smoke: {}", if ok { "PASS" } else { "FAIL" });
        std::process::exit(if ok { 0 } else { 1 });
    }

    /// NSTextFieldDelegate hook: while the field editor has focus, Escape
    /// arrives here as cancelOperation: before reaching the window.
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
        } else {
            NO
        }
    }
}
