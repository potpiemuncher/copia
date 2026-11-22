# Contributing to Copia

Thanks for looking. Copia is small on purpose: one Rust crate, one HTML file,
no frontend build step. Most contributions are a new provider or a fix to an
existing one.

## Build and test

You need [Rust](https://rustup.rs). On Linux also:

```bash
sudo apt install libwebkit2gtk-4.1-dev libayatana-appindicator3-dev librsvg2-dev libxdo-dev
```

```bash
cargo test
```

```bash
cargo run -- --demo
```

`--demo` uses made-up numbers and touches no credentials. `--print` fetches
once and prints the quotas as JSON, which is the quickest way to check a
provider against your own account.

To work on the popover alone, serve `ui/` with any static server and open
`index.html`; `ui/preview.js` stands in for the app. Add `?stress` to the URL
for long labels, a long error and enough providers to wrap.

## Adding or fixing a provider

A provider is one function in `src/providers.rs` that returns
`Ok(Some(quotas))`, `Ok(None)` when the user is not signed in, or `Err(message)`.

- Read the credential the vendor's own tool already left on the machine.
  Never write or refresh it, and send it only to that vendor.
- Put the parsing in its own function and test it against a real or
  documented reply, with anything secret removed.
- Say in the pull request how you verified it: a live account, the vendor's
  documentation, or another client's source.
- A provider nobody has run against a live account goes in `EXPERIMENTAL`, so
  the app labels it "unverified". Remove it from that list once someone has.

## Ground rules

- No telemetry, no accounts, no server. Nothing about the user leaves the
  machine for Copia's benefit.
- Never paste tokens, cookies or `auth.json` contents into issues, pull
  requests or tests.
- Keep the README's Status section true. If something is unverified, it says so.
- The popover must keep working in light and dark themes, with a keyboard,
  and with a screen reader.

## Pull requests

`main` is protected: every change lands through a pull request, and the build
must pass on macOS, Windows and Linux before it can merge. Small and focused
is easiest to review; please make sure `cargo test` passes first.

## Releases

Maintainers release by setting the version in `Cargo.toml` and
`tauri.conf.json`, merging, then pushing a tag such as `v0.2.0`. The `release`
workflow builds the Windows, macOS and Linux installers and publishes them.

By contributing you agree your work is released under the [MIT license](LICENSE)
and that you will follow the [Code of Conduct](CODE_OF_CONDUCT.md).
