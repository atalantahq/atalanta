//! `docs/DSL-REFERENCE.md` is generated from the schema (U2): this fails
//! when the checked-in file differs from what the code would write, so the
//! reference can't drift from the DSL again. `ATALANTA_BLESS=1` rewrites it.

#[test]
fn the_checked_in_dsl_reference_is_current() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/DSL-REFERENCE.md");
    let want = atalanta_core::reference::dsl_reference();
    if std::env::var_os("ATALANTA_BLESS").is_some() {
        std::fs::write(&path, &want).unwrap();
        return;
    }
    let have = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        have == want,
        "docs/DSL-REFERENCE.md is out of date with the schema; regenerate it with \
         `ATALANTA_BLESS=1 cargo test -p atalanta-core --test dsl_reference` \
         (or `atalanta schema --markdown > docs/DSL-REFERENCE.md`)"
    );
}
