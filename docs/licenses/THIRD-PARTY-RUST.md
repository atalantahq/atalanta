# Third-party Rust dependencies

Generated from `Cargo.lock` by `scripts/third-party-rust.py` (`cargo metadata --locked --all-features`: every target platform, dev dependencies included). Do not edit by hand: run `scripts/third-party-rust.py --write` after dependency changes (CI fails when this file is stale). `cargo deny check licenses` (see `deny.toml`) enforces the allow-list.

Atalanta itself is AGPL-3.0-only. Where a crate offers a choice of licences (for example `MIT OR Apache-2.0`), Atalanta uses it under a permissive option.

Crates whose choice includes a copyleft option Atalanta does not take: `r-efi 5.3.0` (`MIT OR Apache-2.0 OR LGPL-2.1-or-later`), `r-efi 6.0.0` (`MIT OR Apache-2.0 OR LGPL-2.1-or-later`), `self_cell 1.2.2` (`Apache-2.0 OR GPL-2.0-only`).

Crates under a copyleft licence only (used unmodified as libraries): `colored 2.2.0` (`MPL-2.0`).

Crates listed: 363 (excluding the 17 workspace crates).

## Count by license expression

| License expression | Crates |
| --- | ---: |
| `MIT OR Apache-2.0` | 192 |
| `MIT` | 64 |
| `Apache-2.0 OR MIT` | 18 |
| `Unicode-3.0` | 18 |
| `Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT` | 14 |
| `MIT/Apache-2.0` | 14 |
| `Apache-2.0` | 4 |
| `Apache-2.0 OR ISC OR MIT` | 3 |
| `BSD-3-Clause` | 3 |
| `Unlicense OR MIT` | 3 |
| `Zlib` | 3 |
| `Zlib OR Apache-2.0 OR MIT` | 3 |
| `BSD-2-Clause OR Apache-2.0 OR MIT` | 2 |
| `ISC` | 2 |
| `MIT OR Apache-2.0 OR LGPL-2.1-or-later` | 2 |
| `MIT OR Apache-2.0 OR Zlib` | 2 |
| `Unlicense/MIT` | 2 |
| `(Apache-2.0 OR MIT) AND BSD-3-Clause` | 1 |
| `(MIT OR Apache-2.0) AND Unicode-3.0` | 1 |
| `0BSD OR MIT OR Apache-2.0` | 1 |
| `Apache-2.0 / MIT` | 1 |
| `Apache-2.0 AND ISC` | 1 |
| `Apache-2.0 OR BSL-1.0` | 1 |
| `Apache-2.0 OR GPL-2.0-only` | 1 |
| `Apache-2.0/MIT` | 1 |
| `BSD-2-Clause` | 1 |
| `CC0-1.0 OR MIT-0 OR Apache-2.0` | 1 |
| `CDLA-Permissive-2.0` | 1 |
| `MIT AND BSD-3-Clause` | 1 |
| `MIT OR Zlib OR Apache-2.0` | 1 |
| `MPL-2.0` | 1 |

## Crates

| Crate | Version | License | Repository |
| --- | --- | --- | --- |
| adler2 | 2.0.1 | `0BSD OR MIT OR Apache-2.0` | https://github.com/oyvindln/adler2 |
| aho-corasick | 1.1.4 | `Unlicense OR MIT` | https://github.com/BurntSushi/aho-corasick |
| android_system_properties | 0.1.5 | `MIT/Apache-2.0` | https://github.com/nical/android_system_properties |
| anstream | 0.6.21 | `MIT OR Apache-2.0` | https://github.com/rust-cli/anstyle.git |
| anstyle | 1.0.13 | `MIT OR Apache-2.0` | https://github.com/rust-cli/anstyle.git |
| anstyle-parse | 0.2.7 | `MIT OR Apache-2.0` | https://github.com/rust-cli/anstyle.git |
| anstyle-query | 1.1.5 | `MIT OR Apache-2.0` | https://github.com/rust-cli/anstyle.git |
| anstyle-wincon | 3.0.11 | `MIT OR Apache-2.0` | https://github.com/rust-cli/anstyle.git |
| anyhow | 1.0.104 | `MIT OR Apache-2.0` | https://github.com/dtolnay/anyhow |
| arrayref | 0.3.9 | `BSD-2-Clause` | https://github.com/droundy/arrayref |
| arrayvec | 0.7.6 | `MIT OR Apache-2.0` | https://github.com/bluss/arrayvec |
| async-trait | 0.1.89 | `MIT OR Apache-2.0` | https://github.com/dtolnay/async-trait |
| async-tungstenite | 0.32.1 | `MIT` | https://github.com/sdroege/async-tungstenite |
| atomic-waker | 1.1.2 | `Apache-2.0 OR MIT` | https://github.com/smol-rs/atomic-waker |
| autocfg | 1.5.0 | `Apache-2.0 OR MIT` | https://github.com/cuviper/autocfg |
| axum | 0.8.8 | `MIT` | https://github.com/tokio-rs/axum |
| axum-core | 0.5.6 | `MIT` | https://github.com/tokio-rs/axum |
| base64 | 0.22.1 | `MIT OR Apache-2.0` | https://github.com/marshallpierce/rust-base64 |
| base64 | 0.23.1 | `MIT OR Apache-2.0` | https://github.com/marshallpierce/rust-base64 |
| bit-set | 0.11.1 | `Apache-2.0 OR MIT` | https://github.com/contain-rs/bit-vec |
| bit-vec | 0.10.1 | `Apache-2.0 OR MIT` | https://github.com/contain-rs/bit-vec |
| bitflags | 1.3.2 | `MIT/Apache-2.0` | https://github.com/bitflags/bitflags |
| bitflags | 2.11.0 | `MIT OR Apache-2.0` | https://github.com/bitflags/bitflags |
| block-buffer | 0.10.4 | `MIT OR Apache-2.0` | https://github.com/RustCrypto/utils |
| borsh | 1.8.1 | `MIT OR Apache-2.0` | https://github.com/near/borsh-rs |
| borsh-derive | 1.8.1 | `Apache-2.0` | https://github.com/near/borsh-rs |
| bumpalo | 3.20.2 | `MIT OR Apache-2.0` | https://github.com/fitzgen/bumpalo |
| bytemuck | 1.25.0 | `Zlib OR Apache-2.0 OR MIT` | https://github.com/Lokathor/bytemuck |
| bytemuck_derive | 1.10.2 | `Zlib OR Apache-2.0 OR MIT` | https://github.com/Lokathor/bytemuck |
| bytes | 1.11.1 | `MIT` | https://github.com/tokio-rs/bytes |
| cc | 1.2.56 | `MIT OR Apache-2.0` | https://github.com/rust-lang/cc-rs |
| cfg-if | 1.0.4 | `MIT OR Apache-2.0` | https://github.com/rust-lang/cfg-if |
| cfg_aliases | 0.2.2 | `MIT` | https://github.com/katharostech/cfg_aliases |
| chacha20 | 0.10.2 | `MIT OR Apache-2.0` | https://github.com/RustCrypto/stream-ciphers |
| chromiumoxide | 0.9.1 | `MIT OR Apache-2.0` | https://github.com/mattsse/chromiumoxide |
| chromiumoxide_cdp | 0.9.1 | `MIT OR Apache-2.0` | https://github.com/mattsse/chromiumoxide |
| chromiumoxide_pdl | 0.9.1 | `MIT OR Apache-2.0` | https://github.com/mattsse/chromiumoxide |
| chromiumoxide_types | 0.9.1 | `MIT OR Apache-2.0` | https://github.com/mattsse/chromiumoxide |
| chrono | 0.4.44 | `MIT OR Apache-2.0` | https://github.com/chronotope/chrono |
| clap | 4.5.60 | `MIT OR Apache-2.0` | https://github.com/clap-rs/clap |
| clap_builder | 4.5.60 | `MIT OR Apache-2.0` | https://github.com/clap-rs/clap |
| clap_derive | 4.5.55 | `MIT OR Apache-2.0` | https://github.com/clap-rs/clap |
| clap_lex | 1.0.0 | `MIT OR Apache-2.0` | https://github.com/clap-rs/clap |
| colorchoice | 1.0.4 | `MIT OR Apache-2.0` | https://github.com/rust-cli/anstyle.git |
| colored | 2.2.0 | `MPL-2.0` | https://github.com/mackwic/colored |
| cookie | 0.18.3 | `MIT OR Apache-2.0` | https://github.com/SergioBenitez/cookie-rs |
| cookie_store | 0.22.1 | `MIT OR Apache-2.0` | https://github.com/pfernie/cookie_store |
| core-foundation | 0.10.1 | `MIT OR Apache-2.0` | https://github.com/servo/core-foundation-rs |
| core-foundation-sys | 0.8.7 | `MIT OR Apache-2.0` | https://github.com/servo/core-foundation-rs |
| core_detect | 1.0.0 | `MIT/Apache-2.0` | https://github.com/thomcc/core_detect |
| core_maths | 0.1.1 | `MIT` | https://github.com/robertbastian/core_maths |
| cosmic-text | 0.16.0 | `MIT OR Apache-2.0` | https://github.com/pop-os/cosmic-text |
| cpufeatures | 0.2.17 | `MIT OR Apache-2.0` | https://github.com/RustCrypto/utils |
| cpufeatures | 0.3.1 | `MIT OR Apache-2.0` | https://github.com/RustCrypto/utils |
| crc32fast | 1.5.0 | `MIT OR Apache-2.0` | https://github.com/srijs/rust-crc32fast |
| crypto-common | 0.1.7 | `MIT OR Apache-2.0` | https://github.com/RustCrypto/traits |
| data-encoding | 2.10.0 | `MIT` | https://github.com/ia0/data-encoding |
| deranged | 0.5.8 | `MIT OR Apache-2.0` | https://github.com/jhpratt/deranged |
| digest | 0.10.7 | `MIT OR Apache-2.0` | https://github.com/RustCrypto/traits |
| displaydoc | 0.2.5 | `MIT OR Apache-2.0` | https://github.com/yaahc/displaydoc |
| document-features | 0.2.12 | `MIT OR Apache-2.0` | https://github.com/slint-ui/document-features |
| dunce | 1.0.5 | `CC0-1.0 OR MIT-0 OR Apache-2.0` | https://gitlab.com/kornelski/dunce |
| either | 1.15.0 | `MIT OR Apache-2.0` | https://github.com/rayon-rs/either |
| encoding_rs | 0.8.35 | `(Apache-2.0 OR MIT) AND BSD-3-Clause` | https://github.com/hsivonen/encoding_rs |
| equivalent | 1.0.2 | `Apache-2.0 OR MIT` | https://github.com/indexmap-rs/equivalent |
| errno | 0.3.14 | `MIT OR Apache-2.0` | https://github.com/lambda-fairy/rust-errno |
| fastrand | 2.3.0 | `Apache-2.0 OR MIT` | https://github.com/smol-rs/fastrand |
| fdeflate | 0.3.7 | `MIT OR Apache-2.0` | https://github.com/image-rs/fdeflate |
| find-msvc-tools | 0.1.9 | `MIT OR Apache-2.0` | https://github.com/rust-lang/cc-rs |
| flate2 | 1.1.9 | `MIT OR Apache-2.0` | https://github.com/rust-lang/flate2-rs |
| fnv | 1.0.7 | `Apache-2.0 / MIT` | https://github.com/servo/rust-fnv |
| foldhash | 0.1.5 | `Zlib` | https://github.com/orlp/foldhash |
| font-types | 0.10.1 | `MIT OR Apache-2.0` | https://github.com/googlefonts/fontations |
| fontconfig-parser | 0.5.8 | `MIT` | https://github.com/Riey/fontconfig-parser |
| fontdb | 0.23.0 | `MIT` | https://github.com/RazrFalcon/fontdb |
| form_urlencoded | 1.2.3 | `MIT OR Apache-2.0` | https://github.com/servo/rust-url |
| futures | 0.3.32 | `MIT OR Apache-2.0` | https://github.com/rust-lang/futures-rs |
| futures-channel | 0.3.32 | `MIT OR Apache-2.0` | https://github.com/rust-lang/futures-rs |
| futures-core | 0.3.32 | `MIT OR Apache-2.0` | https://github.com/rust-lang/futures-rs |
| futures-executor | 0.3.32 | `MIT OR Apache-2.0` | https://github.com/rust-lang/futures-rs |
| futures-io | 0.3.32 | `MIT OR Apache-2.0` | https://github.com/rust-lang/futures-rs |
| futures-macro | 0.3.32 | `MIT OR Apache-2.0` | https://github.com/rust-lang/futures-rs |
| futures-sink | 0.3.32 | `MIT OR Apache-2.0` | https://github.com/rust-lang/futures-rs |
| futures-task | 0.3.32 | `MIT OR Apache-2.0` | https://github.com/rust-lang/futures-rs |
| futures-timer | 3.0.3 | `MIT/Apache-2.0` | https://github.com/async-rs/futures-timer |
| futures-util | 0.3.32 | `MIT OR Apache-2.0` | https://github.com/rust-lang/futures-rs |
| generic-array | 0.14.7 | `MIT` | https://github.com/fizyk20/generic-array.git |
| getrandom | 0.2.17 | `MIT OR Apache-2.0` | https://github.com/rust-random/getrandom |
| getrandom | 0.3.4 | `MIT OR Apache-2.0` | https://github.com/rust-random/getrandom |
| getrandom | 0.4.2 | `MIT OR Apache-2.0` | https://github.com/rust-random/getrandom |
| grid | 1.0.1 | `MIT` | https://github.com/becheran/grid |
| h2 | 0.4.20 | `MIT` | https://github.com/hyperium/h2 |
| harfrust | 0.4.1 | `MIT` | https://github.com/harfbuzz/harfrust |
| hashbrown | 0.15.5 | `MIT OR Apache-2.0` | https://github.com/rust-lang/hashbrown |
| hashbrown | 0.16.1 | `MIT OR Apache-2.0` | https://github.com/rust-lang/hashbrown |
| heck | 0.5.0 | `MIT OR Apache-2.0` | https://github.com/withoutboats/heck |
| http | 1.4.0 | `MIT OR Apache-2.0` | https://github.com/hyperium/http |
| http-body | 1.0.1 | `MIT` | https://github.com/hyperium/http-body |
| http-body-util | 0.1.3 | `MIT` | https://github.com/hyperium/http-body |
| httparse | 1.10.1 | `MIT OR Apache-2.0` | https://github.com/seanmonstar/httparse |
| httpdate | 1.0.3 | `MIT OR Apache-2.0` | https://github.com/pyfisch/httpdate |
| hyper | 1.12.0 | `MIT` | https://github.com/hyperium/hyper |
| hyper-rustls | 0.27.7 | `Apache-2.0 OR ISC OR MIT` | https://github.com/rustls/hyper-rustls |
| hyper-util | 0.1.20 | `MIT` | https://github.com/hyperium/hyper-util |
| iana-time-zone | 0.1.65 | `MIT OR Apache-2.0` | https://github.com/strawlab/iana-time-zone |
| iana-time-zone-haiku | 0.1.2 | `MIT OR Apache-2.0` | https://github.com/strawlab/iana-time-zone |
| icu_collections | 2.1.1 | `Unicode-3.0` | https://github.com/unicode-org/icu4x |
| icu_locale_core | 2.1.1 | `Unicode-3.0` | https://github.com/unicode-org/icu4x |
| icu_normalizer | 2.1.1 | `Unicode-3.0` | https://github.com/unicode-org/icu4x |
| icu_normalizer_data | 2.1.1 | `Unicode-3.0` | https://github.com/unicode-org/icu4x |
| icu_properties | 2.1.2 | `Unicode-3.0` | https://github.com/unicode-org/icu4x |
| icu_properties_data | 2.1.2 | `Unicode-3.0` | https://github.com/unicode-org/icu4x |
| icu_provider | 2.1.1 | `Unicode-3.0` | https://github.com/unicode-org/icu4x |
| id-arena | 2.3.0 | `MIT/Apache-2.0` | https://github.com/fitzgen/id-arena |
| idna | 1.1.0 | `MIT OR Apache-2.0` | https://github.com/servo/rust-url/ |
| idna_adapter | 1.2.1 | `Apache-2.0 OR MIT` | https://github.com/hsivonen/idna_adapter |
| indexmap | 2.13.0 | `Apache-2.0 OR MIT` | https://github.com/indexmap-rs/indexmap |
| inventory | 0.3.25 | `MIT OR Apache-2.0` | https://github.com/dtolnay/inventory |
| ipnet | 2.12.0 | `MIT OR Apache-2.0` | https://github.com/krisprice/ipnet |
| iri-string | 0.7.10 | `MIT OR Apache-2.0` | https://github.com/lo48576/iri-string |
| is_terminal_polyfill | 1.70.2 | `MIT OR Apache-2.0` | https://github.com/polyfill-rs/is_terminal_polyfill |
| itoa | 1.0.17 | `MIT OR Apache-2.0` | https://github.com/dtolnay/itoa |
| js-sys | 0.3.91 | `MIT OR Apache-2.0` | https://github.com/wasm-bindgen/wasm-bindgen/tree/master/crates/js-sys |
| lazy_static | 1.5.0 | `MIT OR Apache-2.0` | https://github.com/rust-lang-nursery/lazy-static.rs |
| leb128fmt | 0.1.0 | `MIT OR Apache-2.0` | https://github.com/bluk/leb128fmt |
| libc | 0.2.191 | `MIT OR Apache-2.0` | https://github.com/rust-lang/libc |
| libm | 0.2.16 | `MIT` | https://github.com/rust-lang/compiler-builtins |
| linebender_resource_handle | 0.1.1 | `Apache-2.0 OR MIT` | https://github.com/linebender/raw_resource_handle |
| linux-raw-sys | 0.12.1 | `Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT` | https://github.com/sunfishcode/linux-raw-sys |
| litemap | 0.8.1 | `Unicode-3.0` | https://github.com/unicode-org/icu4x |
| litrs | 1.0.1 | `MIT OR Apache-2.0` | https://github.com/LukasKalbertodt/litrs |
| lock_api | 0.4.14 | `MIT OR Apache-2.0` | https://github.com/Amanieu/parking_lot |
| log | 0.4.29 | `MIT OR Apache-2.0` | https://github.com/rust-lang/log |
| lru-slab | 0.1.3 | `MIT OR Apache-2.0 OR Zlib` | https://github.com/Ralith/lru-slab |
| matchers | 0.2.0 | `MIT` | https://github.com/hawkw/matchers |
| matchit | 0.8.4 | `MIT AND BSD-3-Clause` | https://github.com/ibraheemdev/matchit |
| memchr | 2.8.0 | `Unlicense OR MIT` | https://github.com/BurntSushi/memchr |
| memmap2 | 0.9.12 | `MIT OR Apache-2.0` | https://github.com/RazrFalcon/memmap2-rs |
| mime | 0.3.17 | `MIT OR Apache-2.0` | https://github.com/hyperium/mime |
| mime_guess | 2.0.5 | `MIT` | https://github.com/abonander/mime_guess |
| minimal-lexical | 0.2.1 | `MIT/Apache-2.0` | https://github.com/Alexhuszagh/minimal-lexical |
| miniz_oxide | 0.8.9 | `MIT OR Zlib OR Apache-2.0` | https://github.com/Frommi/miniz_oxide/tree/master/miniz_oxide |
| mio | 1.2.4 | `MIT` | https://github.com/tokio-rs/mio |
| nix | 0.31.3 | `MIT` | https://github.com/nix-rust/nix |
| nom | 7.1.3 | `MIT` | https://github.com/Geal/nom |
| nu-ansi-term | 0.50.3 | `MIT` | https://github.com/nushell/nu-ansi-term |
| num-conv | 0.2.0 | `MIT OR Apache-2.0` | https://github.com/jhpratt/num-conv |
| num-traits | 0.2.19 | `MIT OR Apache-2.0` | https://github.com/rust-num/num-traits |
| once_cell | 1.21.3 | `MIT OR Apache-2.0` | https://github.com/matklad/once_cell |
| once_cell_polyfill | 1.70.2 | `MIT OR Apache-2.0` | https://github.com/polyfill-rs/once_cell_polyfill |
| openssl-probe | 0.2.2 | `MIT OR Apache-2.0` | https://github.com/rustls/openssl-probe |
| parking_lot | 0.12.5 | `MIT OR Apache-2.0` | https://github.com/Amanieu/parking_lot |
| parking_lot_core | 0.9.12 | `MIT OR Apache-2.0` | https://github.com/Amanieu/parking_lot |
| percent-encoding | 2.3.2 | `MIT OR Apache-2.0` | https://github.com/servo/rust-url/ |
| pin-project-lite | 0.2.17 | `Apache-2.0 OR MIT` | https://github.com/taiki-e/pin-project-lite |
| plist | 1.10.0 | `MIT` | https://github.com/ebarnard/rust-plist/ |
| png | 0.17.16 | `MIT OR Apache-2.0` | https://github.com/image-rs/image-png |
| png | 0.18.1 | `MIT OR Apache-2.0` | https://github.com/image-rs/image-png |
| potential_utf | 0.1.4 | `Unicode-3.0` | https://github.com/unicode-org/icu4x |
| powerfmt | 0.2.0 | `MIT OR Apache-2.0` | https://github.com/jhpratt/powerfmt |
| ppv-lite86 | 0.2.21 | `MIT OR Apache-2.0` | https://github.com/cryptocorrosion/cryptocorrosion |
| prettyplease | 0.2.37 | `MIT OR Apache-2.0` | https://github.com/dtolnay/prettyplease |
| proc-macro-crate | 3.5.0 | `MIT OR Apache-2.0` | https://github.com/bkchr/proc-macro-crate |
| proc-macro2 | 1.0.106 | `MIT OR Apache-2.0` | https://github.com/dtolnay/proc-macro2 |
| proptest | 1.12.0 | `MIT OR Apache-2.0` | https://github.com/proptest-rs/proptest |
| psl-types | 2.0.11 | `MIT/Apache-2.0` | https://github.com/addr-rs/psl-types |
| publicsuffix | 2.3.0 | `MIT/Apache-2.0` | https://github.com/rushmorem/publicsuffix |
| quick-error | 1.2.3 | `MIT/Apache-2.0` | http://github.com/tailhook/quick-error |
| quick-xml | 0.41.0 | `MIT` | https://github.com/tafia/quick-xml |
| quinn | 0.11.12 | `MIT OR Apache-2.0` | https://github.com/quinn-rs/quinn |
| quinn-proto | 0.11.19 | `MIT OR Apache-2.0` | https://github.com/quinn-rs/quinn |
| quinn-udp | 0.5.16 | `MIT OR Apache-2.0` | https://github.com/quinn-rs/quinn |
| quote | 1.0.45 | `MIT OR Apache-2.0` | https://github.com/dtolnay/quote |
| r-efi | 5.3.0 | `MIT OR Apache-2.0 OR LGPL-2.1-or-later` | https://github.com/r-efi/r-efi |
| r-efi | 6.0.0 | `MIT OR Apache-2.0 OR LGPL-2.1-or-later` | https://github.com/r-efi/r-efi |
| rand | 0.9.5 | `MIT OR Apache-2.0` | https://github.com/rust-random/rand |
| rand | 0.10.3 | `MIT OR Apache-2.0` | https://github.com/rust-random/rand |
| rand_chacha | 0.9.0 | `MIT OR Apache-2.0` | https://github.com/rust-random/rand |
| rand_core | 0.9.5 | `MIT OR Apache-2.0` | https://github.com/rust-random/rand |
| rand_core | 0.10.1 | `MIT OR Apache-2.0` | https://github.com/rust-random/rand_core |
| rand_pcg | 0.10.2 | `MIT OR Apache-2.0` | https://github.com/rust-random/rngs |
| rand_xorshift | 0.5.0 | `MIT OR Apache-2.0` | https://github.com/rust-random/rngs |
| rangemap | 1.7.1 | `MIT/Apache-2.0` | https://github.com/jeffparsons/rangemap |
| read-fonts | 0.35.0 | `MIT OR Apache-2.0` | https://github.com/googlefonts/fontations |
| read-fonts | 0.36.0 | `MIT OR Apache-2.0` | https://github.com/googlefonts/fontations |
| redox_syscall | 0.5.18 | `MIT` | https://gitlab.redox-os.org/redox-os/syscall |
| regex | 1.12.3 | `MIT OR Apache-2.0` | https://github.com/rust-lang/regex |
| regex-automata | 0.4.14 | `MIT OR Apache-2.0` | https://github.com/rust-lang/regex |
| regex-syntax | 0.8.10 | `MIT OR Apache-2.0` | https://github.com/rust-lang/regex |
| reqwest | 0.12.28 | `MIT OR Apache-2.0` | https://github.com/seanmonstar/reqwest |
| reqwest | 0.13.5 | `MIT OR Apache-2.0` | https://github.com/seanmonstar/reqwest |
| ring | 0.17.14 | `Apache-2.0 AND ISC` | https://github.com/briansmith/ring |
| roxmltree | 0.20.0 | `MIT OR Apache-2.0` | https://github.com/RazrFalcon/roxmltree |
| rust-embed | 8.11.0 | `MIT` | https://pyrossh.dev/repos/rust-embed |
| rust-embed-impl | 8.11.0 | `MIT` | https://pyrossh.dev/repos/rust-embed |
| rust-embed-utils | 8.11.0 | `MIT` | https://pyrossh.dev/repos/rust-embed |
| rustc-hash | 1.1.0 | `Apache-2.0/MIT` | https://github.com/rust-lang-nursery/rustc-hash |
| rustc-hash | 2.1.3 | `Apache-2.0 OR MIT` | https://github.com/rust-lang/rustc-hash |
| rustix | 1.1.6 | `Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT` | https://github.com/bytecodealliance/rustix |
| rustls | 0.23.46 | `Apache-2.0 OR ISC OR MIT` | https://github.com/rustls/rustls |
| rustls-native-certs | 0.8.5 | `Apache-2.0 OR ISC OR MIT` | https://github.com/rustls/rustls-native-certs |
| rustls-pki-types | 1.14.0 | `MIT OR Apache-2.0` | https://github.com/rustls/pki-types |
| rustls-webpki | 0.103.16 | `ISC` | https://github.com/rustls/webpki |
| rustversion | 1.0.22 | `MIT OR Apache-2.0` | https://github.com/dtolnay/rustversion |
| rusty-fork | 0.3.1 | `MIT/Apache-2.0` | https://github.com/altsysrq/rusty-fork |
| ryu | 1.0.23 | `Apache-2.0 OR BSL-1.0` | https://github.com/dtolnay/ryu |
| same-file | 1.0.6 | `Unlicense/MIT` | https://github.com/BurntSushi/same-file |
| schannel | 0.1.30 | `MIT` | https://github.com/steffengy/schannel-rs |
| scopeguard | 1.2.0 | `MIT OR Apache-2.0` | https://github.com/bluss/scopeguard |
| security-framework | 3.7.0 | `MIT OR Apache-2.0` | https://github.com/kornelski/rust-security-framework |
| security-framework-sys | 2.17.0 | `MIT OR Apache-2.0` | https://github.com/kornelski/rust-security-framework |
| self_cell | 1.2.2 | `Apache-2.0 OR GPL-2.0-only` | https://github.com/Voultapher/self_cell |
| semver | 1.0.27 | `MIT OR Apache-2.0` | https://github.com/dtolnay/semver |
| serde | 1.0.228 | `MIT OR Apache-2.0` | https://github.com/serde-rs/serde |
| serde_core | 1.0.228 | `MIT OR Apache-2.0` | https://github.com/serde-rs/serde |
| serde_derive | 1.0.228 | `MIT OR Apache-2.0` | https://github.com/serde-rs/serde |
| serde_json | 1.0.149 | `MIT OR Apache-2.0` | https://github.com/serde-rs/json |
| serde_json_path | 0.7.2 | `MIT` | https://github.com/hiltontj/serde_json_path |
| serde_json_path_core | 0.2.2 | `MIT` | https://github.com/hiltontj/serde_json_path |
| serde_json_path_macros | 0.1.6 | `MIT` | https://github.com/hiltontj/serde_json_path |
| serde_json_path_macros_internal | 0.1.2 | `MIT` | https://github.com/hiltontj/serde_json_path |
| serde_path_to_error | 0.1.20 | `MIT OR Apache-2.0` | https://github.com/dtolnay/path-to-error |
| serde_urlencoded | 0.7.1 | `MIT/Apache-2.0` | https://github.com/nox/serde_urlencoded |
| serde_yaml | 0.9.34+deprecated | `MIT OR Apache-2.0` | https://github.com/dtolnay/serde-yaml |
| sha1 | 0.10.6 | `MIT OR Apache-2.0` | https://github.com/RustCrypto/hashes |
| sha2 | 0.10.9 | `MIT OR Apache-2.0` | https://github.com/RustCrypto/hashes |
| sharded-slab | 0.1.7 | `MIT` | https://github.com/hawkw/sharded-slab |
| shlex | 1.3.0 | `MIT OR Apache-2.0` | https://github.com/comex/rust-shlex |
| signal-hook-registry | 1.4.8 | `MIT OR Apache-2.0` | https://github.com/vorner/signal-hook |
| simd-adler32 | 0.3.8 | `MIT` | https://github.com/mcountryman/simd-adler32 |
| skrifa | 0.37.0 | `MIT OR Apache-2.0` | https://github.com/googlefonts/fontations |
| skrifa | 0.39.0 | `MIT OR Apache-2.0` | https://github.com/googlefonts/fontations |
| slab | 0.4.12 | `MIT` | https://github.com/tokio-rs/slab |
| slotmap | 1.1.1 | `Zlib` | https://github.com/orlp/slotmap |
| smallvec | 1.15.1 | `MIT OR Apache-2.0` | https://github.com/servo/rust-smallvec |
| smol_str | 0.2.2 | `MIT OR Apache-2.0` | https://github.com/rust-analyzer/smol_str |
| socket2 | 0.6.3 | `MIT OR Apache-2.0` | https://github.com/rust-lang/socket2 |
| stable_deref_trait | 1.2.1 | `MIT OR Apache-2.0` | https://github.com/storyyeller/stable_deref_trait |
| strict-num | 0.1.1 | `MIT` | https://github.com/RazrFalcon/strict-num |
| strsim | 0.11.1 | `MIT` | https://github.com/rapidfuzz/strsim-rs |
| subtle | 2.6.1 | `BSD-3-Clause` | https://github.com/dalek-cryptography/subtle |
| swash | 0.2.6 | `Apache-2.0 OR MIT` | https://github.com/dfrg/swash |
| syn | 2.0.117 | `MIT OR Apache-2.0` | https://github.com/dtolnay/syn |
| syn | 3.0.9 | `MIT OR Apache-2.0` | https://github.com/dtolnay/syn |
| sync_wrapper | 1.0.2 | `Apache-2.0` | https://github.com/Actyx/sync_wrapper |
| synstructure | 0.13.2 | `MIT` | https://github.com/mystor/synstructure |
| sys-locale | 0.3.2 | `MIT OR Apache-2.0` | https://github.com/1Password/sys-locale |
| taffy | 0.9.2 | `MIT` | https://github.com/DioxusLabs/taffy |
| tempfile | 3.27.0 | `MIT OR Apache-2.0` | https://github.com/Stebalien/tempfile |
| thiserror | 2.0.18 | `MIT OR Apache-2.0` | https://github.com/dtolnay/thiserror |
| thiserror-impl | 2.0.18 | `MIT OR Apache-2.0` | https://github.com/dtolnay/thiserror |
| thread_local | 1.1.9 | `MIT OR Apache-2.0` | https://github.com/Amanieu/thread_local-rs |
| time | 0.3.47 | `MIT OR Apache-2.0` | https://github.com/time-rs/time |
| time-core | 0.1.8 | `MIT OR Apache-2.0` | https://github.com/time-rs/time |
| time-macros | 0.2.27 | `MIT OR Apache-2.0` | https://github.com/time-rs/time |
| tiny-skia | 0.11.4 | `BSD-3-Clause` | https://github.com/RazrFalcon/tiny-skia |
| tiny-skia-path | 0.11.4 | `BSD-3-Clause` | https://github.com/RazrFalcon/tiny-skia/tree/master/path |
| tinystr | 0.8.2 | `Unicode-3.0` | https://github.com/unicode-org/icu4x |
| tinyvec | 1.11.0 | `Zlib OR Apache-2.0 OR MIT` | https://github.com/Lokathor/tinyvec |
| tinyvec_macros | 0.1.1 | `MIT OR Apache-2.0 OR Zlib` | https://github.com/Soveu/tinyvec_macros |
| tokio | 1.54.1 | `MIT` | https://github.com/tokio-rs/tokio |
| tokio-macros | 2.7.2 | `MIT` | https://github.com/tokio-rs/tokio |
| tokio-rustls | 0.26.4 | `MIT OR Apache-2.0` | https://github.com/rustls/tokio-rustls |
| tokio-tungstenite | 0.28.0 | `MIT` | https://github.com/snapview/tokio-tungstenite |
| tokio-util | 0.7.18 | `MIT` | https://github.com/tokio-rs/tokio |
| toml_datetime | 1.1.2+spec-1.1.0 | `MIT OR Apache-2.0` | https://github.com/toml-rs/toml |
| toml_edit | 0.25.17+spec-1.1.0 | `MIT OR Apache-2.0` | https://github.com/toml-rs/toml |
| toml_parser | 1.1.5+spec-1.1.0 | `MIT OR Apache-2.0` | https://github.com/toml-rs/toml |
| toml_writer | 1.1.3+spec-1.1.0 | `MIT OR Apache-2.0` | https://github.com/toml-rs/toml |
| tower | 0.5.3 | `MIT` | https://github.com/tower-rs/tower |
| tower-http | 0.6.8 | `MIT` | https://github.com/tower-rs/tower-http |
| tower-layer | 0.3.3 | `MIT` | https://github.com/tower-rs/tower |
| tower-service | 0.3.3 | `MIT` | https://github.com/tower-rs/tower |
| tracing | 0.1.44 | `MIT` | https://github.com/tokio-rs/tracing |
| tracing-attributes | 0.1.31 | `MIT` | https://github.com/tokio-rs/tracing |
| tracing-core | 0.1.36 | `MIT` | https://github.com/tokio-rs/tracing |
| tracing-log | 0.2.0 | `MIT` | https://github.com/tokio-rs/tracing |
| tracing-serde | 0.2.0 | `MIT` | https://github.com/tokio-rs/tracing |
| tracing-subscriber | 0.3.22 | `MIT` | https://github.com/tokio-rs/tracing |
| try-lock | 0.2.5 | `MIT` | https://github.com/seanmonstar/try-lock |
| ttf-parser | 0.25.1 | `MIT OR Apache-2.0` | https://github.com/harfbuzz/ttf-parser |
| tungstenite | 0.28.0 | `MIT OR Apache-2.0` | https://github.com/snapview/tungstenite-rs |
| typed-path | 0.13.0 | `MIT OR Apache-2.0` | https://github.com/chipsenkbeil/typed-path |
| typenum | 1.19.0 | `MIT OR Apache-2.0` | https://github.com/paholg/typenum |
| unarray | 0.1.4 | `MIT OR Apache-2.0` | https://github.com/cameron1024/unarray |
| unicase | 2.9.0 | `MIT OR Apache-2.0` | https://github.com/seanmonstar/unicase |
| unicode-bidi | 0.3.18 | `MIT OR Apache-2.0` | https://github.com/servo/unicode-bidi |
| unicode-ident | 1.0.24 | `(MIT OR Apache-2.0) AND Unicode-3.0` | https://github.com/dtolnay/unicode-ident |
| unicode-linebreak | 0.1.5 | `Apache-2.0` | https://github.com/axelf4/unicode-linebreak |
| unicode-script | 0.5.8 | `MIT OR Apache-2.0` | https://github.com/unicode-rs/unicode-script |
| unicode-segmentation | 1.12.0 | `MIT OR Apache-2.0` | https://github.com/unicode-rs/unicode-segmentation |
| unicode-xid | 0.2.6 | `MIT OR Apache-2.0` | https://github.com/unicode-rs/unicode-xid |
| unsafe-libyaml | 0.2.11 | `MIT` | https://github.com/dtolnay/unsafe-libyaml |
| untrusted | 0.9.0 | `ISC` | https://github.com/briansmith/untrusted |
| url | 2.5.9 | `MIT OR Apache-2.0` | https://github.com/servo/rust-url |
| utf-8 | 0.7.6 | `MIT OR Apache-2.0` | https://github.com/SimonSapin/rust-utf8 |
| utf8_iter | 1.0.4 | `Apache-2.0 OR MIT` | https://github.com/hsivonen/utf8_iter |
| utf8parse | 0.2.2 | `Apache-2.0 OR MIT` | https://github.com/alacritty/vte |
| uuid | 1.29.2 | `Apache-2.0 OR MIT` | https://github.com/uuid-rs/uuid |
| valuable | 0.1.1 | `MIT` | https://github.com/tokio-rs/valuable |
| version_check | 0.9.5 | `MIT/Apache-2.0` | https://github.com/SergioBenitez/version_check |
| wait-timeout | 0.2.1 | `MIT/Apache-2.0` | https://github.com/alexcrichton/wait-timeout |
| walkdir | 2.5.0 | `Unlicense/MIT` | https://github.com/BurntSushi/walkdir |
| want | 0.3.2 | `MIT` | https://github.com/seanmonstar/want |
| wasi | 0.11.1+wasi-snapshot-preview1 | `Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT` | https://github.com/bytecodealliance/wasi |
| wasip2 | 1.0.2+wasi-0.2.9 | `Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT` | https://github.com/bytecodealliance/wasi-rs |
| wasip3 | 0.4.0+wasi-0.3.0-rc-2026-01-06 | `Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT` | https://github.com/bytecodealliance/wasi-rs |
| wasm-bindgen | 0.2.114 | `MIT OR Apache-2.0` | https://github.com/wasm-bindgen/wasm-bindgen |
| wasm-bindgen-futures | 0.4.64 | `MIT OR Apache-2.0` | https://github.com/wasm-bindgen/wasm-bindgen/tree/master/crates/futures |
| wasm-bindgen-macro | 0.2.114 | `MIT OR Apache-2.0` | https://github.com/wasm-bindgen/wasm-bindgen/tree/master/crates/macro |
| wasm-bindgen-macro-support | 0.2.114 | `MIT OR Apache-2.0` | https://github.com/wasm-bindgen/wasm-bindgen/tree/master/crates/macro-support |
| wasm-bindgen-shared | 0.2.114 | `MIT OR Apache-2.0` | https://github.com/wasm-bindgen/wasm-bindgen/tree/master/crates/shared |
| wasm-encoder | 0.244.0 | `Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT` | https://github.com/bytecodealliance/wasm-tools/tree/main/crates/wasm-encoder |
| wasm-metadata | 0.244.0 | `Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT` | https://github.com/bytecodealliance/wasm-tools/tree/main/crates/wasm-metadata |
| wasmparser | 0.244.0 | `Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT` | https://github.com/bytecodealliance/wasm-tools/tree/main/crates/wasmparser |
| web-sys | 0.3.91 | `MIT OR Apache-2.0` | https://github.com/wasm-bindgen/wasm-bindgen/tree/master/crates/web-sys |
| web-time | 1.1.0 | `MIT OR Apache-2.0` | https://github.com/daxpedda/web-time |
| webpki-roots | 1.0.11 | `CDLA-Permissive-2.0` | https://github.com/rustls/webpki-roots |
| which | 8.0.6 | `MIT` | https://github.com/harryfei/which-rs.git |
| winapi-util | 0.1.11 | `Unlicense OR MIT` | https://github.com/BurntSushi/winapi-util |
| windows-core | 0.62.2 | `MIT OR Apache-2.0` | https://github.com/microsoft/windows-rs |
| windows-implement | 0.60.2 | `MIT OR Apache-2.0` | https://github.com/microsoft/windows-rs |
| windows-interface | 0.59.3 | `MIT OR Apache-2.0` | https://github.com/microsoft/windows-rs |
| windows-link | 0.2.1 | `MIT OR Apache-2.0` | https://github.com/microsoft/windows-rs |
| windows-registry | 0.6.1 | `MIT OR Apache-2.0` | https://github.com/microsoft/windows-rs |
| windows-result | 0.4.1 | `MIT OR Apache-2.0` | https://github.com/microsoft/windows-rs |
| windows-strings | 0.5.1 | `MIT OR Apache-2.0` | https://github.com/microsoft/windows-rs |
| windows-sys | 0.52.0 | `MIT OR Apache-2.0` | https://github.com/microsoft/windows-rs |
| windows-sys | 0.59.0 | `MIT OR Apache-2.0` | https://github.com/microsoft/windows-rs |
| windows-sys | 0.61.2 | `MIT OR Apache-2.0` | https://github.com/microsoft/windows-rs |
| windows-targets | 0.52.6 | `MIT OR Apache-2.0` | https://github.com/microsoft/windows-rs |
| windows_aarch64_gnullvm | 0.52.6 | `MIT OR Apache-2.0` | https://github.com/microsoft/windows-rs |
| windows_aarch64_msvc | 0.52.6 | `MIT OR Apache-2.0` | https://github.com/microsoft/windows-rs |
| windows_i686_gnu | 0.52.6 | `MIT OR Apache-2.0` | https://github.com/microsoft/windows-rs |
| windows_i686_gnullvm | 0.52.6 | `MIT OR Apache-2.0` | https://github.com/microsoft/windows-rs |
| windows_i686_msvc | 0.52.6 | `MIT OR Apache-2.0` | https://github.com/microsoft/windows-rs |
| windows_x86_64_gnu | 0.52.6 | `MIT OR Apache-2.0` | https://github.com/microsoft/windows-rs |
| windows_x86_64_gnullvm | 0.52.6 | `MIT OR Apache-2.0` | https://github.com/microsoft/windows-rs |
| windows_x86_64_msvc | 0.52.6 | `MIT OR Apache-2.0` | https://github.com/microsoft/windows-rs |
| winnow | 1.0.4 | `MIT` | https://github.com/winnow-rs/winnow |
| wit-bindgen | 0.51.0 | `Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT` | https://github.com/bytecodealliance/wit-bindgen |
| wit-bindgen-core | 0.51.0 | `Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT` | https://github.com/bytecodealliance/wit-bindgen |
| wit-bindgen-rust | 0.51.0 | `Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT` | https://github.com/bytecodealliance/wit-bindgen |
| wit-bindgen-rust-macro | 0.51.0 | `Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT` | https://github.com/bytecodealliance/wit-bindgen |
| wit-component | 0.244.0 | `Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT` | https://github.com/bytecodealliance/wasm-tools/tree/main/crates/wit-component |
| wit-parser | 0.244.0 | `Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT` | https://github.com/bytecodealliance/wasm-tools/tree/main/crates/wit-parser |
| writeable | 0.6.2 | `Unicode-3.0` | https://github.com/unicode-org/icu4x |
| yazi | 0.2.1 | `Apache-2.0 OR MIT` | https://github.com/dfrg/yazi |
| yoke | 0.8.1 | `Unicode-3.0` | https://github.com/unicode-org/icu4x |
| yoke-derive | 0.8.1 | `Unicode-3.0` | https://github.com/unicode-org/icu4x |
| zeno | 0.3.3 | `Apache-2.0 OR MIT` | https://github.com/dfrg/zeno |
| zerocopy | 0.8.42 | `BSD-2-Clause OR Apache-2.0 OR MIT` | https://github.com/google/zerocopy |
| zerocopy-derive | 0.8.42 | `BSD-2-Clause OR Apache-2.0 OR MIT` | https://github.com/google/zerocopy |
| zerofrom | 0.1.6 | `Unicode-3.0` | https://github.com/unicode-org/icu4x |
| zerofrom-derive | 0.1.6 | `Unicode-3.0` | https://github.com/unicode-org/icu4x |
| zeroize | 1.8.2 | `Apache-2.0 OR MIT` | https://github.com/RustCrypto/utils |
| zerotrie | 0.2.3 | `Unicode-3.0` | https://github.com/unicode-org/icu4x |
| zerovec | 0.11.5 | `Unicode-3.0` | https://github.com/unicode-org/icu4x |
| zerovec-derive | 0.11.2 | `Unicode-3.0` | https://github.com/unicode-org/icu4x |
| zip | 9.1.3 | `MIT` | https://github.com/zip-rs/zip2 |
| zlib-rs | 0.6.8 | `Zlib` | https://github.com/trifectatechfoundation/zlib-rs |
| zmij | 1.0.21 | `MIT` | https://github.com/dtolnay/zmij |
| zopfli | 0.8.3 | `Apache-2.0` | https://github.com/zopfli-rs/zopfli |
