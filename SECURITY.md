# Security policy

## Supported versions

Only the latest 0.x release gets security fixes.

## Report a vulnerability

Do not open a public issue. Use GitHub's private reporting instead:
go to the [Security tab](https://github.com/b-luu/apisip/security) and choose
"Report a vulnerability".

Include the apisip version, what you did, and what you saw. Do not include
real credentials.

You can expect an answer within 7 days. If the report is valid, we agree on a
fix and a disclosure date with you, and we credit you in the release notes
unless you prefer not to be named.

## Scope

apisip sends the credentials in `.env` to the API in `apisip.toml`. Reports
about leaking those credentials (in logs, error messages, redirects or
next-page links to other hosts) are in scope.
