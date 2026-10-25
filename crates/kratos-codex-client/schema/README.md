# Codex app-server schema

`codex_app_server_protocol.schemas.json` is the bundle that Codex 0.159.1 writes with

```sh
codex app-server generate-json-schema --experimental --out <folder>
```

It is committed unchanged, so a diff against a new export shows what a Codex upgrade changes.
`build.rs` generates the Rust types from it. Kratos uses this bundle and not the
`codex_app_server_protocol.v2.schemas.json` file from the same export, because only this bundle
holds the server requests Kratos must answer, such as the approval requests.

To move to a newer Codex, export again, replace the file, and fix what the build and the tests
report.
