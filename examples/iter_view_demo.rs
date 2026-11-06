//! Runnable demo of the RUST-2541 prototype changes: `src/raw/view.rs` (the element view API)
//! against today's `src/raw/iter.rs` behavior.
//!
//! Run with: `cargo run --example iter_view_demo`
//!
//! Part 1 shows today's API stopping at the first invalid-UTF-8 key.
//! Part 2 shows `iter_element_views` + `value_view` handing the same
//! document's raw bytes to the caller, with the shape parsed.
//! Part 3 shows validation as explicit, fallible steps.

use bson::raw::{RawBsonRefView, RawDocument, RawElement};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // {"hello": "world", <invalid-utf8 key>: "bad key", "s": "h<invalid>llo",
    //  "re": /<invalid>p/i, "cws": code "c<invalid>" with scope {"x": 1}}
    fn len_encode(payload: &[u8]) -> Vec<u8> {
        let mut out = ((payload.len() + 1) as i32).to_le_bytes().to_vec();
        out.extend(payload);
        out.push(0);
        out
    }
    fn cstr(payload: &[u8]) -> Vec<u8> {
        let mut out = payload.to_vec();
        out.push(0);
        out
    }
    fn doc_bytes(elements: &[u8]) -> Vec<u8> {
        let mut out = ((elements.len() + 5) as i32).to_le_bytes().to_vec();
        out.extend(elements);
        out.push(0);
        out
    }
    // render bytes readably: a quoted string when valid UTF-8, a byte list otherwise
    fn show(bytes: &[u8]) -> String {
        match std::str::from_utf8(bytes) {
            Ok(s) => format!("{s:?}"),
            Err(_) => format!("{bytes:?}"),
        }
    }

    let mut scope = Vec::new();
    scope.push(0x10);
    scope.extend(cstr(b"x"));
    scope.extend(1i32.to_le_bytes());
    let scope = doc_bytes(&scope);
    let code = len_encode(b"c\xff");

    let mut elements = Vec::new();
    elements.push(0x02);
    elements.extend(cstr(b"hello"));
    elements.extend(len_encode(b"world"));
    elements.push(0x02);
    elements.extend(cstr(b"\xff\xfe"));
    elements.extend(len_encode(b"bad key"));
    elements.push(0x02);
    elements.extend(cstr(b"s"));
    elements.extend(len_encode(b"h\xffllo"));
    elements.push(0x0B);
    elements.extend(cstr(b"re"));
    elements.extend(cstr(b"\xffp"));
    elements.extend(cstr(b"i"));
    elements.push(0x0F);
    elements.extend(cstr(b"cws"));
    let mut value = ((4 + code.len() + scope.len()) as i32)
        .to_le_bytes()
        .to_vec();
    value.extend(&code);
    value.extend(&scope);
    elements.extend(&value);

    let bytes = doc_bytes(&elements);
    let doc = RawDocument::from_bytes(&bytes)?;

    println!("--- 1. today's API: iter_elements() ---");
    for element in doc.iter_elements() {
        match element {
            Ok(element) => println!("  key {} ok", show(element.key().as_str().as_bytes())),
            Err(error) => {
                println!("  ERROR: {error}");
                println!("  ... iteration dead, remaining elements unreachable\n");
                break;
            }
        }
    }

    println!("--- 2. prototype: iter_element_views() + value_view() ---");
    for element in doc.iter_element_views() {
        let element = element?;
        print!(
            "  {:?} key={}",
            element.element_type(),
            show(element.key_bytes())
        );
        match element.value_view()? {
            RawBsonRefView::String(bytes) => println!(" value=String({})", show(bytes)),
            RawBsonRefView::RegularExpression { pattern, options } => {
                println!(" value=/ {} / {}", show(pattern), show(options))
            }
            RawBsonRefView::JavaScriptCodeWithScope { code, scope } => {
                println!(" value=code({}) scope:", show(code));
                for scoped in scope.iter_element_views() {
                    let scoped = scoped?;
                    let RawBsonRefView::Int32(v) = scoped.value_view()? else {
                        panic!("unexpected");
                    };
                    println!(
                        "      {:?} key={} value=Int32({v})",
                        scoped.element_type(),
                        show(scoped.key_bytes())
                    );
                }
            }
            other => println!(" value={other:?}"),
        }
    }

    println!("\n--- 3. validation stays explicit ---");
    let mut iter = doc.iter_element_views();
    let string_element = iter.nth(2).unwrap().unwrap();
    println!(
        "  key={} strict value(): {:?}",
        show(string_element.key_bytes()),
        string_element
            .value()
            .map_err(|error| error.to_string())
            .unwrap_err()
    );
    println!(
        "  key={} value_view() bytes: {:?}",
        show(string_element.key_bytes()),
        match string_element.value_view()? {
            RawBsonRefView::String(bytes) => bytes,
            _ => unreachable!(),
        }
    );
    let bad_key_element = doc.iter_element_views().nth(1).unwrap().unwrap();
    println!(
        "  key={} TryFrom::<RawElement>: {}",
        show(bad_key_element.key_bytes()),
        match TryInto::<RawElement>::try_into(bad_key_element) {
            Ok(element) => format!("ok ({})", show(element.key().as_str().as_bytes())),
            Err(error) => format!("ERROR: {error}"),
        }
    );
    Ok(())
}
