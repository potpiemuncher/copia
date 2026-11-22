# Security

Copia reads sign-in tokens that other tools left on your computer, so reports
about how it handles them are taken seriously.

## Reporting a vulnerability

Please report privately through
[GitHub's private vulnerability reporting](https://github.com/potpiemuncher/copia/security/advisories/new)
or by email to **support@pulsarit.ca**, rather than a public issue. Include what you found, how to reproduce it, and
the version or commit. Never include a real token.

This is a one-maintainer project with no guaranteed response time; you should
normally hear back within a week.

## Supported versions

Only the latest release and the `main` branch are maintained.

## What Copia is meant to guarantee

- Credentials are read only: never written, refreshed, logged or printed.
- Each credential is sent only to the vendor it belongs to, over HTTPS.
- Copia has no server, no account and no telemetry.
- The popover page calls only Copia's own commands and loads no remote content.

Anything that breaks one of these is a vulnerability. So is a way for a
`config.json` custom command or a provider's reply to do more than supply
quota numbers.

## Known limits

- Release builds are not code-signed or notarized.
- The usage endpoints belong to the vendors and are mostly undocumented.
