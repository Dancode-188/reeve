# Security

Reeve proxies API traffic, stores telemetry, and can be configured to
capture message content. If you find a way to make it leak, bypass its
privacy tier, or hurt the machine it runs on, I want to know
privately first.

## Reporting

Use GitHub's private vulnerability reporting on this repository
(Security tab, "Report a vulnerability"). You will get a response
within a week, usually much faster. Give me a way to reproduce the
problem, and if it involves a specific agent or traffic shape, the
closer to a working example the better.

No GitHub account? Mail danbitengo@gmail.com with the same details.

Please do not open a public issue for a vulnerability before we have
had a chance to fix it.

## Scope

Things I consider security bugs: the proxy leaking request or
response content anywhere the privacy tier says it should not go, the
secret scanner writing an actual secret instead of its fingerprint,
tier 1 storing message content, the control channel accepting
commands from off the machine, and anything that lets a watched agent
break out of a kill.

Things that are not: the loopback ports being open on localhost
(deliberate: Reeve binds 127.0.0.1 only), and vulnerabilities in
the agents Reeve watches, which are yours.

## Supported versions

Pre-1.0, only the latest release gets fixes. From 1.0 on, the latest
minor of the current major.
