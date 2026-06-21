//! Emit faithful TLA⁺ encodings (via the `csm_protocol_to_tla` encoder) of representative
//! protocols, for end-to-end TLC validation that the generated modules are real, checkable TLA⁺.
//!
//!   cargo run --release --example tla_encode_demo -- <out-dir>   (default /tmp/tla-out)
//!
//! Then model-check each emitted `<Module>.tla` against its `<Module>.cfg` with TLC.

use std::fs;
use std::path::Path;

use pgmcp::csm::examples::{deliberation, hsm_tool_box, recursive_cf};
use pgmcp::csm::mpst::global::GlobalType;
use pgmcp::csm::registry::protocol_env;
use pgmcp::csm::tla_export::encode_tla;

fn main() {
    let out = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/tla-out".to_string());
    let dir = Path::new(&out);
    fs::create_dir_all(dir).expect("create out dir");
    let env = protocol_env();

    // (module, protocol, .cfg). Module name == file name == TLA+ MODULE name.
    let cases: [(&str, GlobalType, &str); 3] = [
        // Call-free (Choice + Rec): no stack; TLC's deadlock check + state exploration validate it.
        ("Deliberation", deliberation(), "SPECIFICATION Spec\n"),
        // Inline box (HSM): the stack must stay balanced (WellNested) and bounded (StackBounded).
        (
            "Hsm",
            hsm_tool_box(),
            "SPECIFICATION Spec\nCONSTANT MaxStack = 3\nINVARIANT WellNested\nINVARIANT StackBounded\n",
        ),
        // Genuine pushdown recursion: encodes finitely; the same balance/bound obligations hold.
        (
            "recursive_cf",
            recursive_cf(),
            "SPECIFICATION Spec\nCONSTANT MaxStack = 3\nINVARIANT WellNested\nINVARIANT StackBounded\n",
        ),
    ];

    for (module, g, cfg) in cases {
        let tla = encode_tla(&g, &env, module).expect("encode_tla");
        fs::write(dir.join(format!("{module}.tla")), &tla).expect("write .tla");
        fs::write(dir.join(format!("{module}.cfg")), cfg).expect("write .cfg");
        println!("{module}: {} bytes", tla.len());
    }
    println!("wrote 3 modules to {}", dir.display());
}
