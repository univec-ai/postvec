# Security policy

## Supported versions

postvec is in **public beta**. Only the most recent 0.1.x release receives
security fixes; there are no backports to earlier betas.

| Version | Supported |
|---|---|
| latest 0.1.x | yes |
| anything older | no — upgrade first |

## Reporting a vulnerability

Please report suspected vulnerabilities **privately**:

- Preferred: GitHub's private vulnerability reporting on this repository
  (*Security → Report a vulnerability*), or
- Email: **support@univec.ai** with `[SECURITY]` in the subject.

Do **not** open a public issue for an undisclosed vulnerability, and please
give us a reasonable window to ship a fix before any public disclosure. We
will acknowledge a report within five business days.

A useful report includes:

- the postvec version (`SELECT postvec.build_info();` or `postvec --version`)
  and how it was installed (package, image, source);
- the deployment mode (`embedded` or `grpc`) and PostgreSQL major;
- reproduction steps or a proof of concept — ideally against a disposable
  cluster;
- the impact you believe it has (what an attacker gains, and from which
  position: SQL role, network peer, local account, provider endpoint).

## Scope notes worth knowing before reporting

- postvec's generated SQL runs as superuser; anything that lets a
  non-superuser smuggle an identifier or literal past its quoting is in
  scope and high priority.
- The inference gRPC listeners (embedded loopback and `postvec-server`) are
  **deliberately unauthenticated**; the documented deployment requirement is
  a trusted host and a trusted private network. A report that assumes an
  untrusted peer on that network is describing a configuration outside the
  threat model — but a way to *reach* those listeners from an unexpected
  position is very much in scope.
- Provider credentials must never appear in SQL, GUCs, catalogs, logs or
  error messages; any path that leaks one is in scope and high priority.
