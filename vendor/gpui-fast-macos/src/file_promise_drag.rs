//! Finder file-promise drag-out for the macOS platform.
//!
//! The engine advertises a remote (SFTP) selection as
//! [`gpui::ExternalDragPayload::PromisedFiles`]: the receiver names the
//! destination and the application produces the content afterwards. On macOS
//! that is an `NSFilePromiseProvider` per top level item, with a delegate that
//! answers `filePromiseProvider:fileNameForType:` and
//! `filePromiseProvider:writePromiseToURL:completionHandler:`.
//!
//! The provider's `delegate` property is weak, so every delegate object is
//! retained here for as long as Finder may still ask it for content. The write
//! itself runs on a worker thread because the promise provider blocks until the
//! whole file arrived, and the completion handler is invoked back on the GPUI
//! foreground executor so it runs on the main thread as AppKit expects.

// This crate is written against cocoa's AppKit view of the runtime, like the
// path based drag in `window.rs`; using a second, objc2-only call style here
// would be the odd one out.
#![allow(deprecated)]

use std::{
    cell::RefCell,
    ffi::{CStr, OsStr, c_char, c_void},
    io,
    os::unix::ffi::OsStrExt,
    path::PathBuf,
    ptr,
    sync::{
        Arc, Condvar, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};

use block2::{Block, RcBlock};
use cocoa::{
    base::{id, nil},
    foundation::{NSInteger, NSPoint, NSRect, NSSize, NSString, NSUInteger},
};
use futures::channel::oneshot;
use gpui::{ForegroundExecutor, PromisedFileDragPayload, PromisedFileProvider};
use objc::{
    class,
    declare::ClassDecl,
    msg_send,
    runtime::{BOOL, Class, NO, Object, Sel},
    sel, sel_impl,
};
use objc2::runtime::AnyObject;

/// Ivar holding `Arc<PromiseSlot>` as a raw pointer.
const SLOT_IVAR: &str = "promiseSlot";

/// `NSDragOperationCopy`.
const NSDRAG_OPERATION_COPY: NSUInteger = 1;

const PROMISE_ERROR_DOMAIN: &str = "NyaTermFilePromise";

/// A `NSError *` parameter for the completion handler.
type CompletionHandler = dyn Fn(*mut AnyObject);

static mut DELEGATE_CLASS: *const Class = ptr::null();
static mut SOURCE_CLASS: *const Class = ptr::null();

/// One promised top level item, shared by the delegate that answers for it and
/// the gesture that keeps it alive.
struct PromiseSlot {
    name: String,
    provider: Arc<dyn PromisedFileProvider>,
    executor: ForegroundExecutor,
    /// Set once Finder asked for the content, so a later gesture only cancels
    /// the promises nothing ever requested.
    requested: AtomicBool,
}

/// An Objective-C object this module owns a reference to.
///
/// Only touched from the main thread; kept as a raw pointer so the AppKit
/// objects never have to be described by an `objc2` type.
struct OwnedId(id);

impl OwnedId {
    /// Takes over an already owned reference, as returned by `alloc`/`init`.
    fn owned(object: id) -> Self {
        Self(object)
    }
}

impl Drop for OwnedId {
    fn drop(&mut self) {
        let _: () = unsafe { msg_send![self.0, release] };
    }
}

thread_local! {
    /// Gestures whose dragging session already ended.
    ///
    /// Finder reads a promise *after* the drop, so the delegates of a finished
    /// gesture must stay reachable. Two gestures are enough: a promise is
    /// answered within the drop that created it, and anything older has either
    /// been fulfilled or was never requested.
    static RETIRED: RefCell<Vec<(Vec<OwnedId>, Vec<Arc<PromiseSlot>>)>> =
        const { RefCell::new(Vec::new()) };
}

/// Builds the delegate and drag-source classes once.
///
/// # Safety
/// Must run on the main thread before the first promise drag.
pub(crate) unsafe fn build_promise_classes() {
    unsafe {
        if !DELEGATE_CLASS.is_null() {
            return;
        }
        let mut delegate = ClassDecl::new("GPUIWindowFilePromiseDelegate", class!(NSObject))
            .expect("GPUIWindowFilePromiseDelegate is already registered");
        delegate.add_ivar::<*mut c_void>(SLOT_IVAR);
        delegate.add_method(
            sel!(dealloc),
            dealloc_delegate as extern "C" fn(&Object, Sel),
        );
        delegate.add_method(
            sel!(filePromiseProvider:fileNameForType:),
            file_name_for_type as extern "C" fn(&Object, Sel, id, id) -> id,
        );
        delegate.add_method(
            sel!(filePromiseProvider:writePromiseToURL:completionHandler:),
            write_promise_to_url as extern "C" fn(&Object, Sel, id, id, id),
        );
        DELEGATE_CLASS = delegate.register();

        let mut source = ClassDecl::new("GPUIWindowPromiseDragSource", class!(NSObject))
            .expect("GPUIWindowPromiseDragSource is already registered");
        source.add_method(
            sel!(draggingSession:sourceOperationMaskForDraggingContext:),
            source_operation_mask as extern "C" fn(&Object, Sel, id, NSInteger) -> NSUInteger,
        );
        source.add_method(
            sel!(draggingSession:endedAtPoint:operation:),
            source_ended as extern "C" fn(&Object, Sel, id, NSPoint, NSUInteger),
        );
        SOURCE_CLASS = source.register();
    }
}

/// Starts a Finder promise drag for `payload`.
///
/// Returns whether AppKit accepted the dragging session. The caller is
/// responsible for `synthetic_drag_counter` bookkeeping, exactly as for the
/// path based drag.
///
/// # Safety
/// `native_view` and `event` must be the AppKit view and the left-mouse-down
/// `NSEvent` of the gesture, and this must run on the main thread.
pub(crate) unsafe fn start_promise_drag(
    native_view: id,
    event: id,
    payload: &PromisedFileDragPayload,
    executor: ForegroundExecutor,
) -> bool {
    unsafe {
        build_promise_classes();

        let frame = drag_frame(event);
        let items: id = msg_send![class!(NSMutableArray), array];
        let mut objects = Vec::new();
        let mut slots = Vec::new();

        for file in payload.files() {
            let slot = Arc::new(PromiseSlot {
                name: file.name.to_string_lossy().into_owned(),
                provider: file.provider.clone(),
                executor: executor.clone(),
                requested: AtomicBool::new(false),
            });

            let delegate: id = msg_send![DELEGATE_CLASS, alloc];
            let delegate: id = msg_send![delegate, init];
            (*delegate).set_ivar(
                SLOT_IVAR,
                Arc::into_raw(slot.clone()).cast_mut().cast::<c_void>(),
            );
            objects.push(OwnedId::owned(delegate));

            let uti = uti_for(file.is_directory);
            let file_type = crate::ns_string(uti);
            let provider: id = msg_send![class!(NSFilePromiseProvider), alloc];
            let provider: id = msg_send![provider, initWithFileType: file_type delegate: delegate];
            if provider.is_null() {
                log::warn!("promise drag skipped an item AppKit refused");
                continue;
            }
            objects.push(OwnedId::owned(provider));

            // `NSDraggingItem` retains its pasteboard writer, so the provider
            // outlives this frame either way.
            let item: id = msg_send![class!(NSDraggingItem), alloc];
            let item: id = msg_send![item, initWithPasteboardWriter: provider];
            let icon = drag_image(uti);
            let _: () = msg_send![item, setDraggingFrame: frame contents: icon];
            let _: () = msg_send![items, addObject: item];
            let _: () = msg_send![item, release];

            slots.push(slot);
        }

        let count: NSUInteger = msg_send![items, count];
        if count == 0 {
            log::warn!("promise drag declined: no dragging items");
            cancel_unrequested(&slots);
            retire(objects, Vec::new());
            return false;
        }

        let source: id = msg_send![SOURCE_CLASS, alloc];
        let source: id = msg_send![source, init];
        objects.push(OwnedId::owned(source));

        let session: id = msg_send![
            native_view,
            beginDraggingSessionWithItems: items
            event: event
            source: source
        ];

        if session.is_null() {
            log::warn!("promise drag declined: AppKit refused the dragging session");
            cancel_unrequested(&slots);
            retire(objects, Vec::new());
            return false;
        }

        retire(objects, slots);
        true
    }
}

/// Keeps this gesture's objects reachable and releases everything older.
fn retire(objects: Vec<OwnedId>, slots: Vec<Arc<PromiseSlot>>) {
    RETIRED.with(|cell| {
        let mut retired = cell.borrow_mut();
        while retired.len() >= 2 {
            let (_, stale_slots) = retired.remove(0);
            cancel_unrequested(&stale_slots);
        }
        retired.push((objects, slots));
    });
}

/// Cancels promises the receiver never asked for, so an abandoned drag does not
/// pin an SFTP stream.
fn cancel_unrequested(slots: &[Arc<PromiseSlot>]) {
    for slot in slots {
        if !slot.requested.load(Ordering::Relaxed) {
            slot.provider.cancel();
        }
    }
}

fn uti_for(is_directory: bool) -> &'static str {
    if is_directory {
        "public.folder"
    } else {
        "public.data"
    }
}

/// A 32 point drag image anchored on the gesture's location, matching the
/// path based drag.
unsafe fn drag_frame(event: id) -> NSRect {
    let location: NSPoint = unsafe { msg_send![event, locationInWindow] };
    NSRect::new(
        NSPoint::new(location.x - 16., location.y - 16.),
        NSSize::new(32., 32.),
    )
}

/// The Finder icon for `uti`, resolved by type so a slow mount or iCloud path
/// never runs for every selected item during drag startup.
unsafe fn drag_image(uti: &'static str) -> id {
    unsafe {
        let workspace: id = msg_send![class!(NSWorkspace), sharedWorkspace];
        let file_type = crate::ns_string(uti);
        msg_send![workspace, iconForFileType: file_type]
    }
}

unsafe fn slot<'a>(object: &'a Object) -> &'a Arc<PromiseSlot> {
    let raw: *mut c_void = *unsafe { object.get_ivar(SLOT_IVAR) };
    assert!(!raw.is_null(), "promise delegate is missing its slot");
    unsafe { &*raw.cast::<Arc<PromiseSlot>>() }
}

extern "C" fn dealloc_delegate(this: &Object, _: Sel) {
    let raw: *mut c_void = unsafe { *this.get_ivar(SLOT_IVAR) };
    if !raw.is_null() {
        drop(unsafe { Arc::from_raw(raw.cast::<PromiseSlot>()) });
    }
    let _: () = unsafe { msg_send![super(this, class!(NSObject)), dealloc] };
}

extern "C" fn file_name_for_type(this: &Object, _: Sel, _provider: id, _file_type: id) -> id {
    let name = unsafe { NSString::alloc(nil).init_str(&slot(this).name) };
    unsafe { msg_send![name, autorelease] }
}

/// Writes the promised content into the URL Finder chose.
extern "C" fn write_promise_to_url(
    this: &Object,
    _: Sel,
    _provider: id,
    url: id,
    completion_handler: id,
) {
    unsafe {
        let Some(block) = RcBlock::copy(completion_handler.cast::<Block<CompletionHandler>>())
        else {
            log::warn!("promise drag got a null completion handler");
            return;
        };

        let Some(path) = path_from_url(url) else {
            log::warn!("promise drag got a destination that is not a file URL");
            block.call((error_for(&io::Error::other(
                "destination is not a file URL",
            )),));
            return;
        };

        let slot = slot(this).clone();
        slot.requested.store(true, Ordering::Relaxed);

        // `write_to` blocks for the whole transfer, so it cannot run here: this
        // callback arrives on the main thread and would freeze the window.
        let (sender, receiver) = oneshot::channel();
        let provider = slot.provider.clone();
        let spawned = std::thread::Builder::new()
            .name("gpui-file-promise".into())
            .spawn(move || {
                let _permit = acquire_write_permit();
                let _ = sender.send(provider.write_to(&path));
            });
        if let Err(error) = spawned {
            block.call((error_for(&io::Error::other(error)),));
            return;
        }

        slot.executor
            .spawn(async move {
                let result = receiver
                    .await
                    .unwrap_or_else(|_| Err(io::Error::other("the promise writer stopped")));
                match result {
                    Ok(()) => block.call((ptr::null_mut(),)),
                    Err(error) => block.call((error_for(&error),)),
                }
            })
            .detach();
    }
}

/// Waits for one of the three concurrent promise writes this application allows.
///
/// Finder can request every dropped item at once. The transport's drag budget
/// is three operations per source, so the writers queue here rather than
/// starting a thread per file and piling up inside the SFTP session.
fn acquire_write_permit() -> WritePermit {
    const PERMITS: usize = 3;

    static FREE: OnceLock<(Mutex<usize>, Condvar)> = OnceLock::new();
    let (lock, released) = FREE.get_or_init(|| (Mutex::new(PERMITS), Condvar::new()));

    let mut free = lock.lock().unwrap_or_else(|poison| poison.into_inner());
    while *free == 0 {
        free = released
            .wait(free)
            .unwrap_or_else(|poison| poison.into_inner());
    }
    *free -= 1;
    WritePermit { lock, released }
}

struct WritePermit {
    lock: &'static Mutex<usize>,
    released: &'static Condvar,
}

impl Drop for WritePermit {
    fn drop(&mut self) {
        let mut free = self
            .lock
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        *free += 1;
        self.released.notify_one();
    }
}

/// A file URL as a local path, including non-UTF-8 components.
unsafe fn path_from_url(url: id) -> Option<PathBuf> {
    unsafe {
        let is_file: BOOL = msg_send![url, isFileURL];
        if is_file == NO {
            return None;
        }
        let representation: *const c_char = msg_send![url, fileSystemRepresentation];
        if representation.is_null() {
            return None;
        }
        let bytes = CStr::from_ptr(representation).to_bytes();
        Some(PathBuf::from(OsStr::from_bytes(bytes)))
    }
}

/// An `NSError` Finder shows when the write failed.
unsafe fn error_for(error: &io::Error) -> *mut AnyObject {
    unsafe {
        log::warn!("promise drag failed: {error}");
        let domain = crate::ns_string(PROMISE_ERROR_DOMAIN);
        let error: id = msg_send![
            class!(NSError),
            errorWithDomain: domain
            code: 0 as NSInteger
            userInfo: nil
        ];
        error.cast()
    }
}

extern "C" fn source_operation_mask(
    _this: &Object,
    _: Sel,
    _session: id,
    _context: NSInteger,
) -> NSUInteger {
    NSDRAG_OPERATION_COPY
}

extern "C" fn source_ended(
    _this: &Object,
    _: Sel,
    _session: id,
    _point: NSPoint,
    _operation: NSUInteger,
) {
    // Finder resolves the promised content after this callback, so nothing is
    // cancelled here; the gesture retires on the next promise drag.
}
