//! beckon: a keyboard launcher for macOS. Free, open source, airgap pure.
//!
//! This crate is the platform shell: it owns the floating panel, the global
//! hotkey, the indexers, and the Accessibility integrations, all through
//! hand-rolled Objective-C runtime FFI (no wrapper crates). All decisions
//! about what a query means and how results rank live in beckon-core; the
//! engine module wires those decisions to the panel.
//!
//! Flags: `--version` prints and exits; `--smoke` runs the automated shell
//! self-test with no user input: show the panel, round-trip an NSString,
//! then drive the real query pipeline end to end (app search rows, the
//! inline calculator, activation copying to the pasteboard, and frecency
//! persistence under a temp BECKON_HOME), exiting 0 on success;
//! `--index-apps` (macOS only) prints one "title<TAB>id<TAB>path" line per
//! indexed application and exits.
//!
//! On non-macOS targets this builds as a stub so the whole workspace
//! compiles and tests on Linux CI.

#[cfg(target_os = "macos")]
mod apps;
#[cfg(target_os = "macos")]
mod ax;
#[cfg(target_os = "macos")]
mod engine;
#[cfg(target_os = "macos")]
mod ffi;
#[cfg(target_os = "macos")]
mod files;
#[cfg(target_os = "macos")]
mod hotkey;
#[cfg(target_os = "macos")]
mod menubar;
#[cfg(target_os = "macos")]
mod onboarding;
#[cfg(target_os = "macos")]
mod panel;
#[cfg(target_os = "macos")]
mod paste;
#[cfg(target_os = "macos")]
mod pasteboard;
#[cfg(target_os = "macos")]
mod plugins;
#[cfg(target_os = "macos")]
mod scriptcmd;
#[cfg(target_os = "macos")]
mod switcher;
#[cfg(target_os = "macos")]
mod system;
#[cfg(target_os = "macos")]
mod theme;
#[cfg(target_os = "macos")]
mod ui;
#[cfg(target_os = "macos")]
mod winmgmt;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("beckon {}", beckon_core::VERSION);
        return;
    }
    #[cfg(target_os = "macos")]
    if args.iter().any(|a| a == "--index-apps") {
        for item in apps::index() {
            println!("{}\t{}\t{}", item.title, item.id, item.subtitle);
        }
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
    //! --smoke self-test. Feature logic lives in the other modules; this
    //! module only glues them together.

    use crate::ffi::{self, msg, Bool, Id, Sel};
    use crate::{engine, hotkey, panel, ui};
    use beckon_core::frecency::FrecencyStore;
    use std::mem::transmute;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// NSApplicationActivationPolicyAccessory: no Dock icon, no menu bar;
    /// the panel is the whole interface.
    const ACTIVATION_POLICY_ACCESSORY: isize = 1;

    /// The pasteboard type the engine writes calculator results as (the
    /// literal value of NSPasteboardTypeString).
    const PASTEBOARD_TYPE_STRING: &str = "public.utf8-plain-text";

    static SMOKE: AtomicBool = AtomicBool::new(false);
    static SMOKE_SHOWED: AtomicBool = AtomicBool::new(false);
    static SMOKE_SEARCHED: AtomicBool = AtomicBool::new(false);
    static SMOKE_CALCED: AtomicBool = AtomicBool::new(false);
    static SMOKE_FRECENCY: AtomicBool = AtomicBool::new(false);
    static SMOKE_HID: AtomicBool = AtomicBool::new(false);

    pub fn run(smoke: bool) {
        SMOKE.store(smoke, Ordering::Relaxed);
        if smoke {
            // Isolate the smoke run's store: frecency writes land in a
            // temp dir, never in the real ~/.beckon. Set before
            // engine::init reads it.
            let dir = std::env::temp_dir().join(format!("beckon-smoke-{}", std::process::id()));
            std::env::set_var("BECKON_HOME", &dir);
        }
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
            // The engine registers the query and activation callbacks and
            // loads its stores; after this the launcher is fully wired.
            engine::init();
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
                    "beckonSmokeSearch:",
                    transmute::<extern "C" fn(Id, Sel, Id), ffi::Imp>(smoke_search),
                    "v@:@",
                ),
                (
                    "beckonSmokeCalc:",
                    transmute::<extern "C" fn(Id, Sel, Id), ffi::Imp>(smoke_calc),
                    "v@:@",
                ),
                (
                    "beckonSmokeFrecency:",
                    transmute::<extern "C" fn(Id, Sel, Id), ffi::Imp>(smoke_frecency),
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
    /// Summoning goes through the engine so the panel always comes up
    /// with a fresh app index, an empty query, and the frecency list.
    extern "C" fn hotkey_pressed() {
        if panel::is_visible() {
            panel::hide();
        } else {
            engine::summon();
        }
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

    /// Drive the query pipeline exactly the way a user edit does: set the
    /// field text, then post the notification the field editor posts;
    /// NSControl auto-subscribed the ui.rs delegate when panel::init set
    /// it. (Programmatic setStringValue: never fires the callback on its
    /// own.)
    fn type_query(text: &str) {
        panel::set_query(text);
        // Safety: main thread; postNotificationName:object: takes an
        // NSString name and a nullable object.
        unsafe {
            let center = msg!(Id: ffi::class("NSNotificationCenter"), ffi::sel("defaultCenter"));
            msg!((): center, ffi::sel("postNotificationName:object:"),
                Id: ffi::nsstring("NSControlTextDidChangeNotification"),
                Id: panel::field());
        }
    }

    /// Send a command selector through the field delegate's real keyboard
    /// hook, the same path arrow keys and Return take from the field
    /// editor. Returns whether the delegate swallowed it.
    fn send_command(name: &str) -> bool {
        let delegate = ui::field_delegate();
        // Safety: main thread; the delegate implements the method with
        // encoding c@:@@: and tolerates a nil text view.
        unsafe {
            msg!(Bool: delegate,
                ffi::sel("control:textView:doCommandBySelector:"),
                Id: panel::field(), Id: ffi::NIL, Sel: ffi::sel(name))
                != 0
        }
    }

    /// Read the general pasteboard's plain-text contents back via FFI.
    fn read_pasteboard() -> String {
        // Safety: main thread; stringForType: takes an NSString type and
        // returns an NSString or nil, which nsstring_to_string accepts.
        unsafe {
            let pb = msg!(Id: ffi::class("NSPasteboard"), ffi::sel("generalPasteboard"));
            let ns = msg!(Id: pb, ffi::sel("stringForType:"),
                Id: ffi::nsstring(PASTEBOARD_TYPE_STRING));
            ffi::nsstring_to_string(ns)
        }
    }

    /// Step 1: summon through the engine (fresh index, empty query, the
    /// default frecency list showing) and round-trip an NSString through
    /// the query field.
    extern "C" fn smoke_show(this: Id, _sel: Sel, _obj: Id) {
        engine::summon();
        let visible = panel::is_visible();
        let default_rows = ui::row_count();
        panel::set_query("beckon smoke");
        let round_trip = panel::query();
        let ok = visible && default_rows > 0 && round_trip == "beckon smoke";
        SMOKE_SHOWED.store(ok, Ordering::Relaxed);
        println!(
            "smoke: shown visible={visible} default_rows={default_rows} \
             nsstring_roundtrip={round_trip:?}"
        );
        // Safety: as in did_finish_launching.
        unsafe { perform_after(this, "beckonSmokeSearch:", 0.2) };
    }

    /// Step 2: an app search end to end. This machine has Safari, so the
    /// query "safari" must produce rows with Safari on top, and moveDown
    /// must drive the selection through the real delegate hook.
    extern "C" fn smoke_search(this: Id, _sel: Sel, _obj: Id) {
        type_query("safari");
        let rows = ui::row_count();
        let top = ui::row_at(0).unwrap_or_default();
        let down_handled = send_command("moveDown:");
        let selected = ui::selected_index();
        // With a single row, moveDown wraps back to 0; either way the
        // delegate must have swallowed the command.
        let selection_ok = selected == Some(if rows > 1 { 1 } else { 0 });
        // The command sources share the ranked pool with apps: a window
        // action must win its own name.
        type_query("window left half");
        let cmd_top = ui::row_at(0).unwrap_or_default();
        let commands_ok = cmd_top.title == "Window: Left Half";
        // The window switcher trigger: this process has live windows on
        // screen around it, so "win" must list some.
        type_query("win");
        let win_rows = ui::row_count();
        let switcher_ok = win_rows > 0;
        // The M3 triggers, all deterministic core sources: emoji search,
        // a dev utility, the default snippets, and a filled quicklink.
        type_query("emoji fire");
        let emoji_ok = ui::row_at(0).unwrap_or_default().title.contains("fire");
        type_query("uuid");
        let uuid_title = ui::row_at(0).unwrap_or_default().title;
        let uuid_ok =
            ui::row_count() == 1 && uuid_title.len() == 36 && uuid_title.matches('-').count() == 4;
        type_query("snip");
        let snip_ok = ui::row_count() > 0;
        type_query("go google rust launcher");
        let go_sub = ui::row_at(0).unwrap_or_default().subtitle;
        let go_ok = go_sub.contains("q=rust%20launcher");
        let m3_ok = emoji_ok && uuid_ok && snip_ok && go_ok;
        let ok = rows > 0
            && top.title.contains("Safari")
            && down_handled
            && selection_ok
            && commands_ok
            && switcher_ok
            && m3_ok;
        SMOKE_SEARCHED.store(ok, Ordering::Relaxed);
        println!(
            "smoke: search rows={rows} top_title={:?} top_subtitle={:?} \
             moveDown handled={down_handled} selection={selected:?} \
             command_top={:?} switcher_rows={win_rows} emoji_ok={emoji_ok} \
             uuid_ok={uuid_ok} snip_ok={snip_ok} go_ok={go_ok}",
            top.title, top.subtitle, cmd_top.title
        );
        // Safety: as in did_finish_launching.
        unsafe { perform_after(this, "beckonSmokeCalc:", 0.2) };
    }

    /// Step 3: the inline calculator end to end. "2+2" must produce
    /// exactly one row titled "4"; activating it through the real Return
    /// hook must copy "4" to the pasteboard and hide the panel.
    extern "C" fn smoke_calc(this: Id, _sel: Sel, _obj: Id) {
        type_query("2+2");
        let rows = ui::row_count();
        let top = ui::row_at(0).unwrap_or_default();
        let return_handled = send_command("insertNewline:");
        let pasted = read_pasteboard();
        let hidden = !panel::is_visible();
        let ok = rows == 1 && top.title == "4" && return_handled && pasted == "4" && hidden;
        SMOKE_CALCED.store(ok, Ordering::Relaxed);
        println!(
            "smoke: calc rows={rows} top_title={:?} return handled={return_handled} \
             pasteboard={pasted:?} hidden_after_activate={hidden}",
            top.title
        );
        // Safety: as in did_finish_launching.
        unsafe { perform_after(this, "beckonSmokeFrecency:", 0.2) };
    }

    /// Step 4: the frecency persistence path, exactly as an app launch
    /// exercises it (record_use plus atomic save), minus the launch
    /// itself; the file lands under the temp BECKON_HOME and must parse
    /// back with the recorded id scoring above zero.
    extern "C" fn smoke_frecency(this: Id, _sel: Sel, _obj: Id) {
        let recorded = engine::record_use_now("beckon.smoke.item");
        let path = beckon_core::persist::store_root().join("frecency.txt");
        let ok = match (&recorded, std::fs::read_to_string(&path)) {
            (Ok(()), Ok(text)) => match text.parse::<FrecencyStore>() {
                // now = 0 never decays (elapsed saturates at zero), so
                // this reads the raw recorded deposit.
                Ok(store) => store.score("beckon.smoke.item", 0) > 0,
                Err(e) => {
                    eprintln!("smoke: frecency file did not parse: {e}");
                    false
                }
            },
            (Err(e), _) => {
                eprintln!("smoke: record_use_now failed: {e}");
                false
            }
            (_, Err(e)) => {
                eprintln!("smoke: frecency file unreadable: {e}");
                false
            }
        };
        SMOKE_FRECENCY.store(ok, Ordering::Relaxed);
        println!(
            "smoke: frecency recorded={} file={} parsed_score_positive={ok}",
            recorded.is_ok(),
            path.display()
        );
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
        // Clean up the temp store dir the smoke run pointed BECKON_HOME at.
        if let Some(dir) = std::env::var_os("BECKON_HOME") {
            let _ = std::fs::remove_dir_all(dir);
        }
        let ok = SMOKE_SHOWED.load(Ordering::Relaxed)
            && SMOKE_SEARCHED.load(Ordering::Relaxed)
            && SMOKE_CALCED.load(Ordering::Relaxed)
            && SMOKE_FRECENCY.load(Ordering::Relaxed)
            && SMOKE_HID.load(Ordering::Relaxed);
        println!("smoke: {}", if ok { "PASS" } else { "FAIL" });
        std::process::exit(if ok { 0 } else { 1 });
    }
}
