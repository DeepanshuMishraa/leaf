//! Another app's preferences as macOS holds them now, read through
//! CoreFoundation from cfprefsd. An app's changes reach cfprefsd at once but
//! its plist on disk only seconds later, so reading the file lags behind.

use std::ffi::c_void;

type CFTypeRef = *const c_void;
type CFIndex = isize;

const UTF8: u32 = 0x0800_0100;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFPreferencesAnyApplication: CFTypeRef;
    fn CFStringCreateWithBytes(
        alloc: CFTypeRef,
        bytes: *const u8,
        len: CFIndex,
        encoding: u32,
        external: u8,
    ) -> CFTypeRef;
    fn CFStringGetLength(string: CFTypeRef) -> CFIndex;
    fn CFStringGetMaximumSizeForEncoding(length: CFIndex, encoding: u32) -> CFIndex;
    fn CFStringGetCString(string: CFTypeRef, buf: *mut u8, size: CFIndex, encoding: u32) -> u8;
    fn CFStringGetTypeID() -> usize;
    fn CFDataGetTypeID() -> usize;
    fn CFDataGetLength(data: CFTypeRef) -> CFIndex;
    fn CFDataGetBytePtr(data: CFTypeRef) -> *const u8;
    fn CFGetTypeID(value: CFTypeRef) -> usize;
    fn CFRelease(value: CFTypeRef);
    fn CFPreferencesAppSynchronize(app: CFTypeRef) -> u8;
    fn CFPreferencesCopyAppValue(key: CFTypeRef, app: CFTypeRef) -> CFTypeRef;
}

/// A preference value leaf reads: text, or bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pref {
    Text(String),
    Data(Vec<u8>),
}

impl Pref {
    pub fn text(&self) -> Option<&str> {
        match self {
            Pref::Text(text) => Some(text),
            Pref::Data(_) => None,
        }
    }

    pub fn data(&self) -> Option<&[u8]> {
        match self {
            Pref::Data(data) => Some(data),
            Pref::Text(_) => None,
        }
    }
}

/// An owned CoreFoundation object, released when dropped.
struct Owned(CFTypeRef);

impl Owned {
    fn string(text: &str) -> Option<Owned> {
        // SAFETY: the bytes are valid UTF-8 of the given length; the string
        // copies them, and is released once by Drop.
        let string = unsafe {
            CFStringCreateWithBytes(
                std::ptr::null(),
                text.as_ptr(),
                text.len() as CFIndex,
                UTF8,
                0,
            )
        };
        (!string.is_null()).then_some(Owned(string))
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: the object came from a Create or Copy call and is released once.
        unsafe { CFRelease(self.0) }
    }
}

/// Asks cfprefsd again for `app`'s values, dropping what this process cached.
pub fn sync(app: &str) {
    if let Some(app) = Owned::string(app) {
        // SAFETY: `app` is a live CFString.
        unsafe { CFPreferencesAppSynchronize(app.0) };
    }
}

/// `key` in `app`'s preferences, when it's text or data.
pub fn read(app: &str, key: &str) -> Option<Pref> {
    let app = Owned::string(app)?;
    copy(key, app.0)
}

/// `key` in the global preferences every app shares.
pub fn read_global(key: &str) -> Option<Pref> {
    // SAFETY: a constant CoreFoundation provides for the whole process.
    copy(key, unsafe { kCFPreferencesAnyApplication })
}

fn copy(key: &str, app: CFTypeRef) -> Option<Pref> {
    let key = Owned::string(key)?;
    // SAFETY: both are live CFStrings; the result follows the Copy rule, so
    // it's owned here and released by Drop.
    let value = unsafe { CFPreferencesCopyAppValue(key.0, app) };
    if value.is_null() {
        return None;
    }
    let value = Owned(value);
    // SAFETY: the type is checked before each accessor, and the bytes are
    // copied out while the object is alive.
    unsafe {
        let kind = CFGetTypeID(value.0);
        if kind == CFStringGetTypeID() {
            let size = CFStringGetMaximumSizeForEncoding(CFStringGetLength(value.0), UTF8) + 1;
            let mut buf = vec![0u8; size.max(1) as usize];
            if CFStringGetCString(value.0, buf.as_mut_ptr(), size, UTF8) == 0 {
                return None;
            }
            let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            buf.truncate(end);
            String::from_utf8(buf).ok().map(Pref::Text)
        } else if kind == CFDataGetTypeID() {
            let length = CFDataGetLength(value.0).max(0) as usize;
            let bytes = CFDataGetBytePtr(value.0);
            if bytes.is_null() {
                return Some(Pref::Data(Vec::new()));
            }
            Some(Pref::Data(
                std::slice::from_raw_parts(bytes, length).to_vec(),
            ))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_keys_read_as_nothing() {
        assert_eq!(read("dev.leaf.no-such-app", "nothing"), None);
        assert_eq!(read_global("LeafNoSuchKey"), None);
        sync("dev.leaf.no-such-app");
    }

    #[test]
    fn global_text_reads_as_text() {
        // Every Mac has a locale; it's text.
        if let Some(locale) = read_global("AppleLocale") {
            assert!(locale.text().is_some_and(|t| !t.is_empty()), "{locale:?}");
        }
    }
}
