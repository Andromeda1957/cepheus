use super::*;
use crate::testsupport::TempDir;

/// A stand-in for `wl-paste` or `xclip`: lists `types` when asked for them
/// (failing, as both do, when there are none) and prints `type:<type>` for
/// a type's content, so a test sees which type was asked for.
fn fake(dir: &TempDir, name: &str, types: &[&str]) -> String {
    let listed = types.iter().map(|t| format!("{t}\\n")).collect::<String>();
    let script = format!(
        "#!/bin/sh\n\
         for a in \"$@\"; do case \"$a\" in --list-types|TARGETS) [ -n '{listed}' ] || exit 1; printf '{listed}'; exit 0;; esac; done\n\
         while [ $# -gt 0 ]; do case \"$1\" in --type|-t) printf 'type:%s' \"$2\"; exit 0;; esac; shift; done\n\
         exit 1\n"
    );
    dir.executable(name, script).to_string_lossy().into_owned()
}

fn wayland(program: String) -> Reader {
    Reader { wayland: true, wl_paste: program, xclip: "/nonexistent/xclip".into() }
}

#[test]
fn an_image_on_the_clipboard_comes_before_its_text() {
    // A screenshot tool offers the image and often a text type too.
    let dir = TempDir::new("clip-image");
    let reader = wayland(fake(&dir, "wl-paste", &["text/plain;charset=utf-8", "image/jpeg", "image/png"]));
    let Clip::Image { name, bytes } = reader.read().unwrap() else { panic!("an image") };
    assert_eq!(bytes, b"type:image/png", "PNG is preferred");
    assert!(name.starts_with("clipboard-") && name.ends_with(".png"), "{name}");
}

#[test]
fn text_is_read_as_utf8_text() {
    let dir = TempDir::new("clip-text");
    let reader = wayland(fake(&dir, "wl-paste", &["TEXT", "STRING", "text/plain;charset=utf-8", "UTF8_STRING"]));
    assert_eq!(reader.read().unwrap(), Clip::Text("type:text/plain;charset=utf-8".into()));
}

#[test]
fn an_empty_clipboard_is_empty() {
    let dir = TempDir::new("clip-empty");
    assert_eq!(wayland(fake(&dir, "wl-paste", &[])).read().unwrap(), Clip::Empty);
    // Something copied that is neither text nor an image.
    assert_eq!(wayland(fake(&dir, "wl-paste2", &["application/x-gnome-saved"])).read().unwrap(), Clip::Empty);
}

#[test]
fn without_wayland_xclip_reads_the_clipboard_selection() {
    let dir = TempDir::new("clip-x11");
    let xclip = fake(&dir, "xclip", &["TARGETS", "UTF8_STRING", "image/png"]);
    let reader = Reader { wayland: false, wl_paste: "/nonexistent/wl-paste".into(), xclip };
    assert!(matches!(reader.read().unwrap(), Clip::Image { bytes, .. } if bytes == b"type:image/png"));
}

#[test]
fn a_missing_tool_is_an_error_not_an_empty_clipboard() {
    let err = wayland("/nonexistent/wl-paste".into()).read().unwrap_err();
    assert!(format!("{err:#}").contains("could not run /nonexistent/wl-paste"), "{err:#}");
}
