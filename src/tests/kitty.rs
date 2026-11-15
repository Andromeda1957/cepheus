use super::*;

// Kitty drops an escape whose payload passes 4096 bytes, and any reply it
// sends would reach the deck as typed keys, so every part must ask for none.
#[test]
fn animations_split_payloads_and_ask_for_no_reply() {
    let frames = [
        Frame { rgba: vec![7; 9000], width: 50, height: 45, gap: 120 },
        Frame { rgba: vec![1; 9000], width: 50, height: 45, gap: 220 },
    ];
    let text = String::from_utf8(animation(0x0d0701, &frames)).unwrap();
    let parts: Vec<&str> = text.split("\x1b\\").filter(|p| !p.is_empty()).collect();
    assert!(parts.len() > 4);
    for part in &parts {
        let (keys, payload) = part.split_once(';').unwrap_or((part, ""));
        assert!(keys.contains("q=2"), "{keys}");
        assert!(payload.len() <= CHUNK, "{}", payload.len());
    }
    assert!(text.contains("a=f,f=32,i=853761,s=50,v=45,z=220"));
    assert!(text.ends_with("\x1b_Ga=a,i=853761,s=3,v=1,q=2\x1b\\"));
}
