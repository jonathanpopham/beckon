//! Objective-C runtime FFI foundation. Hand-rolled, std only, no wrapper
//! crates. Everything the shell says to AppKit funnels through
//! objc_msgSend.
//!
//! The core technique: objc_msgSend is declared without a signature and
//! transmuted to the exact function type of each call site (the msg!
//! macro). This is the same mechanism every Objective-C binding uses under
//! the hood; the trampoline requires the caller to state the true argument
//! and return types or the call is undefined behavior.
//!
//! Safety invariants for this module, referenced by the unsafe blocks:
//!
//! 1. Signatures: every msg! call site spells the real Objective-C method
//!    signature. A wrong type there is UB, which is why the macro forces
//!    the caller to write every type out.
//! 2. Threading: AppKit is main-thread only. All UI messages in this crate
//!    are sent from the main thread (the app run loop, Carbon hotkey
//!    callbacks, and delayed-perform callbacks all execute there).
//! 3. Lifetimes: classes and selectors live for the whole process. Objects
//!    we create (panel, text field, delegate) are intentionally never
//!    released; they also live as long as the process.

use std::ffi::{c_char, c_void, CStr, CString};

/// Opaque Objective-C object. Only ever used behind a pointer.
#[repr(C)]
pub struct ObjcObject {
    _priv: [u8; 0],
}

/// An object (or class) pointer. Classes are objects in the runtime.
pub type Id = *mut ObjcObject;

/// A selector: an interned method name.
pub type Sel = *const c_void;

/// Objective-C BOOL. One byte on every macOS target we build for.
pub type Bool = i8;

/// An untyped method implementation. Produced by transmuting a concrete
/// extern "C" fn whose signature matches the method's type encoding.
pub type Imp = unsafe extern "C" fn();

pub const YES: Bool = 1;
pub const NO: Bool = 0;
pub const NIL: Id = std::ptr::null_mut();

/// CGFloat is f64 on every 64-bit Apple target.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct NSPoint {
    pub x: f64,
    pub y: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct NSSize {
    pub width: f64,
    pub height: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct NSRect {
    pub origin: NSPoint,
    pub size: NSSize,
}

impl NSRect {
    pub fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            origin: NSPoint { x, y },
            size: NSSize { width, height },
        }
    }
}

/// The first argument to objc_msgSendSuper.
#[repr(C)]
struct ObjcSuper {
    receiver: Id,
    super_class: Id,
}

#[allow(non_snake_case)]
#[link(name = "objc")]
extern "C" {
    /// Declared without a signature on purpose; see the module docs. Only
    /// call through the msg! macro or the typed helpers below.
    pub fn objc_msgSend();
    fn objc_msgSendSuper();
    #[cfg(target_arch = "x86_64")]
    fn objc_msgSend_stret();
    fn objc_getClass(name: *const c_char) -> Id;
    fn sel_registerName(name: *const c_char) -> Sel;
    fn objc_allocateClassPair(superclass: Id, name: *const c_char, extra_bytes: usize) -> Id;
    fn objc_registerClassPair(cls: Id);
    fn class_addMethod(cls: Id, sel: Sel, imp: Imp, types: *const c_char) -> Bool;
    fn objc_autoreleasePoolPush() -> *mut c_void;
    fn objc_autoreleasePoolPop(pool: *mut c_void);
}

// AppKit has no C entry points we call directly; every call goes through
// objc_msgSend. This empty block exists so the framework gets linked and
// its classes are registered with the runtime at load time.
#[link(name = "AppKit", kind = "framework")]
extern "C" {}

/// Typed message send. Spell the return type first, then receiver and
/// selector, then each argument as `type: value`:
///
///     let title = msg!(Id: window, sel("title"));
///     msg!((): window, sel("setLevel:"), isize: 3);
///
/// Safety: the caller must write the method's true signature (invariant 1
/// in the module docs) and, for UI objects, call from the main thread
/// (invariant 2). Every expansion must sit inside an unsafe block.
macro_rules! msg {
    ($ret:ty : $obj:expr, $sel:expr $(, $aty:ty : $arg:expr)* $(,)?) => {{
        let f = ::std::mem::transmute::<
            unsafe extern "C" fn(),
            unsafe extern "C" fn($crate::ffi::Id, $crate::ffi::Sel $(, $aty)*) -> $ret,
        >($crate::ffi::objc_msgSend as unsafe extern "C" fn());
        f($obj, $sel $(, $arg)*)
    }};
}
pub(crate) use msg;

/// Look up a registered class. Panics with a clear message when the class
/// is missing, which means the owning framework did not get linked.
pub fn class(name: &str) -> Id {
    let cname = CString::new(name).expect("class names contain no NUL");
    // Safety: objc_getClass takes a NUL-terminated C string and returns a
    // class pointer or null; both are handled.
    let cls = unsafe { objc_getClass(cname.as_ptr()) };
    assert!(
        !cls.is_null(),
        "Objective-C class {name} not found; is the owning framework linked?"
    );
    cls
}

/// Intern a selector by name.
pub fn sel(name: &str) -> Sel {
    let cname = CString::new(name).expect("selector names contain no NUL");
    // Safety: sel_registerName copies the string and returns an interned,
    // process-lifetime selector.
    unsafe { sel_registerName(cname.as_ptr()) }
}

/// Build an autoreleased NSString from a Rust string. Interior NUL bytes
/// are stripped because C strings cannot carry them.
pub fn nsstring(s: &str) -> Id {
    let owned;
    let clean = if s.contains('\0') {
        owned = s.replace('\0', "");
        owned.as_str()
    } else {
        s
    };
    let cstr = CString::new(clean).expect("interior NULs were just stripped");
    // Safety: stringWithUTF8String: has signature (@:*) returning an
    // autoreleased NSString; the pointer stays valid for the whole call.
    unsafe {
        msg!(Id: class("NSString"), sel("stringWithUTF8String:"), *const c_char: cstr.as_ptr())
    }
}

/// Copy an NSString's contents into an owned Rust String.
///
/// # Safety
/// `ns` must be null or a valid NSString (or NSString-like object that
/// responds to UTF8String).
pub unsafe fn nsstring_to_string(ns: Id) -> String {
    if ns.is_null() {
        return String::new();
    }
    let utf8 = msg!(*const c_char: ns, sel("UTF8String"));
    if utf8.is_null() {
        return String::new();
    }
    CStr::from_ptr(utf8).to_string_lossy().into_owned()
}

/// Send a message that returns an NSRect. Struct returns need their own
/// entry point on x86_64 (objc_msgSend_stret); on arm64 the regular
/// trampoline returns the four doubles in registers.
///
/// # Safety
/// `obj` must be valid and `selector` must name a zero-argument method
/// that really returns an NSRect.
pub unsafe fn msg_send_nsrect(obj: Id, selector: Sel) -> NSRect {
    #[cfg(target_arch = "aarch64")]
    {
        let f = ::std::mem::transmute::<
            unsafe extern "C" fn(),
            unsafe extern "C" fn(Id, Sel) -> NSRect,
        >(objc_msgSend as unsafe extern "C" fn());
        f(obj, selector)
    }
    #[cfg(target_arch = "x86_64")]
    {
        let mut out = NSRect::default();
        let f = ::std::mem::transmute::<
            unsafe extern "C" fn(),
            unsafe extern "C" fn(*mut NSRect, Id, Sel),
        >(objc_msgSend_stret as unsafe extern "C" fn());
        f(&mut out, obj, selector);
        out
    }
}

/// Send a zero-argument, void-returning message to the superclass
/// implementation. Used by runtime subclass overrides that must call
/// through to the framework's version.
///
/// # Safety
/// `receiver` must be an instance whose class inherits from `superclass`,
/// and `selector` must name a method with encoding v@: on that chain.
pub unsafe fn msg_super_void(receiver: Id, superclass: Id, selector: Sel) {
    let sup = ObjcSuper {
        receiver,
        super_class: superclass,
    };
    let f = ::std::mem::transmute::<
        unsafe extern "C" fn(),
        unsafe extern "C" fn(*const ObjcSuper, Sel),
    >(objc_msgSendSuper as unsafe extern "C" fn());
    f(&sup, selector);
}

/// Define and register a new Objective-C class at runtime. Each method is
/// (selector name, implementation, type encoding). Panics loudly if the
/// runtime rejects anything; that is a programming error, not a runtime
/// condition.
///
/// # Safety
/// Every Imp must have been transmuted from an extern "C" fn whose actual
/// signature matches the paired type encoding, and the class name must not
/// already be registered.
pub unsafe fn define_class(name: &str, superclass: &str, methods: &[(&str, Imp, &str)]) -> Id {
    let cname = CString::new(name).expect("class names contain no NUL");
    let cls = objc_allocateClassPair(class(superclass), cname.as_ptr(), 0);
    assert!(
        !cls.is_null(),
        "objc_allocateClassPair failed for {name}; already registered?"
    );
    for (sel_name, imp, encoding) in methods {
        // The runtime does not promise to copy the encoding string, so it
        // is leaked deliberately; class definitions are finite.
        let enc = CString::new(*encoding)
            .expect("type encodings contain no NUL")
            .into_raw();
        let added = class_addMethod(cls, sel(sel_name), *imp, enc);
        assert!(added != 0, "class_addMethod failed for {name} {sel_name}");
    }
    objc_registerClassPair(cls);
    cls
}

/// RAII autorelease pool. Hold one across any stretch of Cocoa calls made
/// outside the app run loop (the run loop drains its own pools per event).
pub struct AutoreleasePool(*mut c_void);

impl AutoreleasePool {
    pub fn new() -> Self {
        // Safety: push is balanced by the pop in Drop.
        Self(unsafe { objc_autoreleasePoolPush() })
    }
}

impl Default for AutoreleasePool {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for AutoreleasePool {
    fn drop(&mut self) {
        // Safety: self.0 came from objc_autoreleasePoolPush and is popped
        // exactly once.
        unsafe { objc_autoreleasePoolPop(self.0) }
    }
}
