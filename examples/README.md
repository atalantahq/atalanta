# Examples

Each folder holds one suite. Run it with `atalanta run examples/<folder>`.

| Folder | Platform | Needs |
|---|---|---|
| [android-example](android-example/atalanta.yaml) | Android Settings app | An emulator (API 33–34, phone layout) |
| [web-example](web-example/atalanta.yaml) | Chrome | Chrome; no network |
| [api-example](api-example/atalanta.yaml) | HTTP only | Network; no device |
| [hybrid-example](hybrid-example/atalanta.yaml) | HTTP + Chrome: checks an API, then finds a captured value on the page | Chrome and network |

More suites: [tests/smoke](../tests/smoke) (Android, iOS and web Settings/smoke runs used in CI).
