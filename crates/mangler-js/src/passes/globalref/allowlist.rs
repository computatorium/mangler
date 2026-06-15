//! Curated allowlist of standard globals that are safe to indirect (`Safe` mode).
//!
//! Membership criteria (both must hold):
//!
//! 1. **Existence.** The name names a standard global that *exists* in its
//!    intended runtime (ECMAScript intrinsic, a Web/DOM API, or a Node global).
//!    This matters because the indirection turns a `ReferenceError` on a missing
//!    global into a silent `undefined` read; restricting to known-present globals
//!    keeps that exposure to programs that were already broken.
//! 2. **Safe receiver semantics.** Reading the name and calling it bare must be
//!    equivalent to reading it from the global object and calling it bare. This
//!    holds for plain functions/constructors/namespaces: `_Ga = globalThis["X"]`
//!    then `_Ga(args)` is an unqualified call of the same value as `X(args)`, so
//!    `this` is identical (undefined strict / global sloppy). Methods that must
//!    be called *with the global object as receiver* (none of the entries below
//!    require this — they are all global values, not methods of a host object
//!    whose `this` is observed) are excluded.
//!
//! `globalThis` itself is intentionally **NOT** on the list: it is the anchor
//! (`var _G = globalThis;`), so indirecting it would be circular.
//!
//! Lookup is an O(log n) binary search over a `&'static [&str]` kept in sorted
//! order (verified by a unit test). No external dependency is needed — `phf` is
//! not in `Cargo.toml`, and a sorted-slice binary search is allocation-free and
//! avoids a `OnceLock`/`HashSet` init.

/// Sorted list of allowlisted global names. MUST stay sorted (a unit test
/// enforces this so `binary_search` stays correct).
static ALLOWLIST: &[&str] = &[
    "AbortController",
    "AbortSignal",
    "AggregateError",
    "AnimationEvent",
    "Array",
    "ArrayBuffer",
    "Atomics",
    "Attr",
    "Audio",
    "AudioContext",
    "BeforeUnloadEvent",
    "BigInt",
    "BigInt64Array",
    "BigUint64Array",
    "Blob",
    "Boolean",
    "BroadcastChannel",
    "Buffer",
    "ByteLengthQueuingStrategy",
    "CSS",
    "CSSStyleDeclaration",
    "CSSStyleSheet",
    "CanvasGradient",
    "CanvasPattern",
    "CanvasRenderingContext2D",
    "CharacterData",
    "ClipboardEvent",
    "CloseEvent",
    "Comment",
    "CompositionEvent",
    "CountQueuingStrategy",
    "Crypto",
    "CryptoKey",
    "CustomElementRegistry",
    "CustomEvent",
    "DOMException",
    "DOMMatrix",
    "DOMParser",
    "DOMPoint",
    "DOMRect",
    "DOMTokenList",
    "DataTransfer",
    "DataView",
    "Date",
    "Document",
    "DocumentFragment",
    "DragEvent",
    "Element",
    "Error",
    "ErrorEvent",
    "EvalError",
    "Event",
    "EventSource",
    "EventTarget",
    "File",
    "FileList",
    "FileReader",
    "FinalizationRegistry",
    "Float16Array",
    "Float32Array",
    "Float64Array",
    "FocusEvent",
    "FontFace",
    "FormData",
    "Function",
    "GamepadEvent",
    "Geolocation",
    "HTMLAnchorElement",
    "HTMLBodyElement",
    "HTMLButtonElement",
    "HTMLCanvasElement",
    "HTMLCollection",
    "HTMLDivElement",
    "HTMLDocument",
    "HTMLElement",
    "HTMLFormElement",
    "HTMLIFrameElement",
    "HTMLImageElement",
    "HTMLInputElement",
    "HTMLLabelElement",
    "HTMLLinkElement",
    "HTMLOptionElement",
    "HTMLScriptElement",
    "HTMLSelectElement",
    "HTMLSpanElement",
    "HTMLStyleElement",
    "HTMLTableElement",
    "HTMLTemplateElement",
    "HTMLTextAreaElement",
    "HTMLVideoElement",
    "HashChangeEvent",
    "Headers",
    "History",
    "IDBDatabase",
    "IDBKeyRange",
    "IDBObjectStore",
    "IDBRequest",
    "IDBTransaction",
    "Image",
    "ImageBitmap",
    "ImageData",
    "Infinity",
    "InputEvent",
    "Int16Array",
    "Int32Array",
    "Int8Array",
    "IntersectionObserver",
    "Intl",
    "JSON",
    "KeyboardEvent",
    "Location",
    "Map",
    "Math",
    "MediaQueryList",
    "MediaRecorder",
    "MediaStream",
    "MessageChannel",
    "MessageEvent",
    "MessagePort",
    "MouseEvent",
    "MutationObserver",
    "NaN",
    "NamedNodeMap",
    "Navigator",
    "Node",
    "NodeFilter",
    "NodeList",
    "Notification",
    "Number",
    "Object",
    "OffscreenCanvas",
    "PerformanceObserver",
    "PointerEvent",
    "PopStateEvent",
    "ProgressEvent",
    "Promise",
    "Proxy",
    "Range",
    "RangeError",
    "ReadableStream",
    "ReferenceError",
    "Reflect",
    "RegExp",
    "Request",
    "ResizeObserver",
    "Response",
    "Screen",
    "ServiceWorker",
    "Set",
    "ShadowRoot",
    "SharedArrayBuffer",
    "Storage",
    "StorageEvent",
    "String",
    "Symbol",
    "SyntaxError",
    "Text",
    "TextDecoder",
    "TextDecoderStream",
    "TextEncoder",
    "TextEncoderStream",
    "TextMetrics",
    "TouchEvent",
    "TransitionEvent",
    "TreeWalker",
    "TypeError",
    "UIEvent",
    "URIError",
    "URL",
    "URLPattern",
    "URLSearchParams",
    "Uint16Array",
    "Uint32Array",
    "Uint8Array",
    "Uint8ClampedArray",
    "WeakMap",
    "WeakRef",
    "WeakSet",
    "WebGL2RenderingContext",
    "WebGLRenderingContext",
    "WebSocket",
    "Wheel",
    "WheelEvent",
    "Window",
    "Worker",
    "WritableStream",
    "XMLHttpRequest",
    "XMLSerializer",
    "XPathEvaluator",
    "XPathResult",
    "__dirname",
    "__filename",
    "alert",
    "atob",
    "btoa",
    "caches",
    "cancelAnimationFrame",
    "cancelIdleCallback",
    "clearImmediate",
    "clearInterval",
    "clearTimeout",
    "close",
    "confirm",
    "console",
    "createImageBitmap",
    "crypto",
    "customElements",
    "decodeURI",
    "decodeURIComponent",
    "dispatchEvent",
    "document",
    "encodeURI",
    "encodeURIComponent",
    "escape",
    "exports",
    "fetch",
    "focus",
    "frames",
    "getComputedStyle",
    "global",
    "history",
    "indexedDB",
    "isFinite",
    "isNaN",
    "localStorage",
    "location",
    "matchMedia",
    "module",
    "navigator",
    "open",
    "parent",
    "parseFloat",
    "parseInt",
    "performance",
    "postMessage",
    "print",
    "process",
    "prompt",
    "queueMicrotask",
    "requestAnimationFrame",
    "requestIdleCallback",
    "require",
    "screen",
    "scroll",
    "scrollBy",
    "scrollTo",
    "self",
    "sessionStorage",
    "setImmediate",
    "setInterval",
    "setTimeout",
    "stop",
    "structuredClone",
    "undefined",
    "unescape",
    "window",
];

/// Returns whether `name` is an allowlisted standard global safe to indirect.
///
/// `globalThis` is never matched (it is the anchor, not an indirection target).
pub fn is_allowlisted(name: &str) -> bool {
    ALLOWLIST.binary_search(&name).is_ok()
}

/// Iterate the allowlist (used by the decoy injector to draw plausible,
/// file-unused global names).
pub fn names() -> &'static [&'static str] {
    ALLOWLIST
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_is_sorted_and_unique() {
        for w in ALLOWLIST.windows(2) {
            assert!(
                w[0] < w[1],
                "allowlist must be strictly sorted (binary_search depends on it); \
                 out of order or duplicate near {:?} / {:?}",
                w[0],
                w[1]
            );
        }
    }

    #[test]
    fn allowlist_excludes_global_this() {
        // The anchor must never be an indirection target.
        assert!(!is_allowlisted("globalThis"));
    }

    #[test]
    fn allowlist_size_in_expected_range() {
        // Spec calls for ~250–350 entries.
        let n = ALLOWLIST.len();
        assert!(n >= 250, "allowlist too small: {n}");
        assert!(n <= 400, "allowlist unexpectedly large: {n}");
    }

    #[test]
    fn known_globals_are_allowlisted() {
        for name in [
            "Object", "Array", "Math", "JSON", "Promise", "parseInt", "isNaN",
            "document", "window", "fetch", "console", "setTimeout", "WebSocket",
            "process", "Buffer", "require",
        ] {
            assert!(is_allowlisted(name), "{name} should be allowlisted");
        }
    }

    #[test]
    fn non_globals_are_not_allowlisted() {
        for name in ["myLocalVar", "foo", "globalThis", "x", "_0x1"] {
            assert!(!is_allowlisted(name), "{name} should NOT be allowlisted");
        }
    }
}
