use super::*;
use crate::testsupport::TempDir;

/// Serve `dir` on a loopback port for the rest of the test process.
fn server(dir: &TempDir) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let dir = dir.path().to_path_buf();
    thread::spawn(move || accept_all(listener, &dir));
    address
}

fn request(address: &str, text: &str) -> Vec<u8> {
    let mut stream = TcpStream::connect(address).unwrap();
    stream.write_all(text.as_bytes()).unwrap();
    let mut out = Vec::new();
    stream.read_to_end(&mut out).unwrap();
    out
}

fn split(response: &[u8]) -> (String, Vec<u8>) {
    let end = response.windows(4).position(|w| w == b"\r\n\r\n").expect("a complete head") + 4;
    (String::from_utf8(response[..end].to_vec()).unwrap(), response[end..].to_vec())
}

#[test]
fn the_apk_is_served_whole_with_its_type_and_length() {
    let dir = TempDir::new("apk-serve");
    let apk: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    fs::write(dir.path().join("agent-deck.apk"), &apk).unwrap(); // legacy-name
    let address = server(&dir);
    let (head, body) = split(&request(&address, "GET /agent-deck.apk HTTP/1.1\r\nHost: x\r\n\r\n")); // legacy-name
    assert!(head.starts_with("HTTP/1.0 200 OK\r\n"), "{head}");
    assert!(head.contains("Content-Type: application/vnd.android.package-archive\r\n"), "{head}");
    assert!(head.contains("Content-Length: 300000\r\n"), "{head}");
    assert!(body == apk, "the bytes on disk, all of them");
    let (head, body) = split(&request(&address, "HEAD /agent-deck.apk?x=1 HTTP/1.1\r\n\r\n")); // legacy-name
    assert!(head.contains("Content-Length: 300000\r\n"), "{head}");
    assert!(body.is_empty(), "HEAD sends no body");
}

#[test]
fn the_index_lists_the_files_but_not_staging_files() {
    let dir = TempDir::new("apk-index");
    dir.write("agent-deck.apk", "apk"); // legacy-name
    dir.write("a b&<.txt", "x");
    dir.write(".agent-deck.apk.partial", "staging"); // legacy-name
    fs::create_dir(dir.path().join("sub")).unwrap();
    let page = index(dir.path());
    assert!(page.contains("<title>Directory listing for /</title>"), "{page}");
    assert!(page.contains("<li><a href=\"a%20b%26%3C.txt\">a b&amp;&lt;.txt</a></li>\n<li><a href=\"agent-deck.apk\">agent-deck.apk</a></li>\n"), "{page}"); // legacy-name
    assert!(!page.contains("partial") && !page.contains("sub"), "{page}");
    let (head, body) = split(&request(&server(&dir), "GET / HTTP/1.0\r\n\r\n"));
    assert!(head.starts_with("HTTP/1.0 200 OK\r\n") && head.contains("text/html"), "{head}");
    assert_eq!(String::from_utf8(body).unwrap(), page);
}

#[test]
fn only_a_file_directly_in_the_folder_is_served() {
    let outer = TempDir::new("apk-outside");
    outer.write("secret", "no");
    let dir = TempDir::new("apk-confined");
    dir.write("ok.apk", "yes");
    dir.write(".hidden", "no");
    std::os::unix::fs::symlink(outer.path().join("secret"), dir.path().join("link")).unwrap();
    let address = server(&dir);
    let (head, body) = split(&request(&address, "GET /ok%2Eapk HTTP/1.1\r\n\r\n"));
    assert!(head.contains("Content-Type: application/vnd.android.package-archive\r\n"), "{head}");
    assert_eq!(body, b"yes");
    for path in ["/link", "/.hidden", "/..%2Fapk-outside%2Fsecret", "/../secret", "/missing", "/sub/ok.apk", "/%zz"] {
        let (head, _) = split(&request(&address, &format!("GET {path} HTTP/1.1\r\n\r\n")));
        assert!(head.starts_with("HTTP/1.0 404 Not Found\r\n"), "{path}: {head}");
    }
    let (head, _) = split(&request(&address, "POST /ok.apk HTTP/1.1\r\nContent-Length: 0\r\n\r\n"));
    assert!(head.starts_with("HTTP/1.0 501 Not Implemented\r\n"), "{head}");
}

#[test]
fn request_names_are_one_decoded_segment() {
    assert_eq!(name("agent-deck.apk").as_deref(), Some("agent-deck.apk")); // legacy-name
    assert_eq!(name("a%20b").as_deref(), Some("a b"));
    for bad in ["", ".x", "a%2Fb", "a/b", "%2E%2E", "a%00", "a%", "a%4", "%ff"] {
        assert_eq!(name(bad), None, "{bad}");
    }
}
